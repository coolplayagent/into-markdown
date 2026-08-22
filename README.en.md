# into-markdown

[中文](README.md)

`into-markdown` is a Rust-first document-to-Markdown conversion platform built
with Bazel. The repository currently contains the architecture, public service
provider interfaces, registry and pipeline, a deterministic GFM renderer,
command-line shell, production TXT/Markdown/CSV/TSV/JSON/XML and character-set converters, a
pinned ONNX Runtime CPU safety layer, PNG/JPEG/TIFF/WebP/BMP image converters, an installable
PP-OCRv6 tiny detection-and-recognition pipeline, and contract tests. A controlled library
transport installs only hash-pinned official ONNX archives, the character table, and reviewed
runtime artifacts. The OpenAI-compatible image-description adapter is available only when
explicitly configured and authorized for the current invocation; network and AI remain off by
default.

The project is implemented independently of the neighbouring `anydoc` and
`markitdown` projects. Documents from every source, including PDF, OCR, and
AI-derived content, pass through one provenance-aware intermediate
representation before GitHub Flavored Markdown (GFM) is produced.

The OCR crate includes bounded PP-OCRv6 tiny text-detection preprocessing and
DB postprocessing over the `TensorRuntime` seam. It accepts only an explicitly
described decoded pixel view (including stride, color layout, and orientation),
produces scored quadrilaterals, angles, and raw-source recognition crop descriptors,
and performs no decoding or I/O. Image decoding remains the responsibility of
the separately audited image-conversion work. The recognition module performs bounded official
perspective cropping, BGR/NCHW preprocessing, stable dynamic batching, strict tensor validation,
and deterministic CTC decoding. The product image engine binds the reviewed official detector and
recognizer artifacts, detector/recognizer identities, geometry, and confidence into structured IR
evidence. Ordinary builds and tests stay offline; explicit manual API and CLI quality targets
install and run both components through the product resolver and native worker over the hash-bound
12-image Simplified Chinese, Traditional Chinese, English, and mixed-language corpus.

## Build

```shell
bazel build //...
bazel test //...
cargo check --workspace
```

The supported targets are macOS ARM64, Linux x86_64, Linux ARM64, and Windows
x86_64. macOS x86_64 is intentionally unsupported. See the
[macOS release](docs/macos-arm64-release.md) and
[Linux/Windows release](docs/platform-modular-release.md) boundaries.

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
bazel run //apps/cli:into-md -- capabilities --json
bazel run //apps/cli:into-md -- capabilities show ocr --json
bazel run //apps/cli:into-md -- setup ocr
bazel run //apps/cli:into-md -- doctor
bazel run //apps/cli:into-md -- ui
```

The CLI accepts conversion inputs directly and has no `convert` subcommand. It
defines batch files and directories, stdin, URIs, OCR/AI routing, structured
JSON, portable bundles, layered configuration, providers, capabilities, and plugins.
Network and AI access are disabled by default; remote sources and providers
require an explicit `--allow-network` on every invocation.

`into-md ui` starts a local Web security entry point and embedded React console
shell fixed to `127.0.0.1`, using
an operating-system-assigned port by default and opening the browser. A fresh
high-entropy session value is handed to the embedded page in the URL fragment;
the API also requires the exact Host, Origin, and session header. The responsive
status page includes themes, Simplified Chinese and English, keyboard and focus
support, and truthfully reports unavailable document capabilities. It does not
include jobs, a database, workbench, preview, or administration. Bazel builds
content-addressed assets offline and embeds them in the Rust binary; no CDN is
used. The checked release inputs include the complete React-family MIT notice
and a deterministic SPDX 2.3 SBOM tied to the exact production app hash. See the Chinese authoritative
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

RSS 2.0 and Atom 1.0 feed conversion is available for already-local or resolved
bytes. It extracts feed and entry titles, authors, timestamps, links, summaries,
and content with original-byte entry provenance. HTML, `content:encoded`, and Atom
title, subtitle, summary, and content HTML/XHTML text constructs share the hardened
HTML converter; filtered active markup is never echoed by a raw-text fallback. Relative URLs are
resolved offline from the source URI and every nested `xml:base`; they are never fetched. Source
order is preserved, RFC 822/RFC 3339 timestamps are parsed strictly with diagnostics,
and duplicates use deterministic ID, canonical-link, then length-framed content-digest
keys. Parsing, nested extraction, deduplication, recursive IR/asset renumbering, and output share
one aggregate logical budget. DTDs, external entities, namespace confusion, and bounded-resource
violations fail closed.
Feed XML expanded names and attributes, `xml:base`/URLs, diagnostics, and event-by-event XHTML
output use the same lease. Owned vectors and strings are charged before allocation using actual
capacity; CDATA and attribute escape growth is measured before writing. This cooperative logical
budget does not claim a physical RSS limit or hard isolation of a third-party allocator.

Markdown/GFM conversion supports headings, emphasis and strikethrough, links and
autolinks, nested and task lists, tables, code blocks, and footnotes while retaining
UTF-8 source byte ranges. A standalone safe HTTP(S) image remains structured as an
external-only Asset without being downloaded; inline, relative, and unsafe targets
produce explicit diagnostics and safe fallbacks. Raw HTML and blockquotes use explicit
non-executable IR fallbacks; see the [format matrix](docs/formats.md) for the full policy.

Local OCR, speech, and legacy Office support ship as three self-contained signed capability
plugins. The OCR plugin owns PP-OCRv6, ONNX Runtime, its character table, and fixed model files;
the speech plugin owns FFmpeg, Whisper, VAD, speaker models, and their runtimes; the legacy Office
plugin owns LibreOffice. Users install, verify, update, and remove each plugin as one unit rather
than managing its models separately. `setup ocr`, `setup media`, and `setup legacy-office` install
and verify official packages; conversion never downloads them implicitly. Local plugins and remote
providers use the same typed routing, fallback, and provenance contract.

The detailed design documents are maintained in Chinese. See the
[architecture](docs/architecture.md), [interface contract](docs/interfaces.md),
[format matrix](docs/formats.md), [OCR and AI design](docs/ocr-and-ai.md),
[OCR and audio capability plugins](docs/capability-plugins.md),
[security model](docs/security.md), and [testing strategy](docs/testing.md).
The authoritative CLI and configuration specifications are maintained in
Chinese: [command-line design](docs/cli.md) and
[configuration](docs/configuration.md). License policy, third-party sources,
and release auditing are covered by [license governance](docs/licensing.md).
## OpenAI-compatible provider security

Provider configuration stores only an API-key environment-variable name. Every provider operation
requires `--allow-network` on that invocation; loopback, link-local, and private addresses also
require `--allow-private-network`. The direct Rustls transport ignores proxy environment variables,
rejects redirects, validates every resolved address, and enforces bounded HTTP, decompression, JSON,
retry, cancellation, and deadline policies. `providers test` sends only a fixed model-list request
and never sends document content.
