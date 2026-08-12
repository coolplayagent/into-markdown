# into-markdown

[中文](README.md)

`into-markdown` is a Rust-first document-to-Markdown conversion platform built
with Bazel. The repository currently contains the architecture, public service
provider interfaces, registry and pipeline, a deterministic GFM renderer,
command-line shell, and contract tests. It does not yet contain production
format parsers, OCR inference, network clients, or LLM calls.

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
bazel run //apps/cli:into-md -- report.pdf -o report.md
bazel run //apps/cli:into-md -- documents/ --recursive --output-dir markdown/
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- models show pp-ocrv6-tiny-zh-en --json
bazel run //apps/cli:into-md -- doctor
```

The CLI accepts conversion inputs directly and has no `convert` subcommand. It
defines batch files and directories, stdin, URIs, OCR/AI routing, structured
JSON, portable bundles, layered configuration, providers, models, and plugins.
Network and AI access are disabled by default; remote sources and providers
require an explicit `--allow-network` on every invocation.

Structured artifacts start at `schemaVersion: 1`. Conversion results, bundles,
and batch reports use public DTOs shared by the CLI and future HTTP service.
For example, `--emit result-json` returns `markdown`, a versioned `document`,
base64 `assets`, `diagnostics`, and `provenance`. The authoritative Chinese
[DTO contract](docs/dto.md) defines compatibility and untrusted JSON budgets.

Model discovery, offline verification, path lookup, and guarded cleanup are
implemented. The authoritative manifests currently contain upstream source
archives but no reviewed final ONNX/character-table runtime files, so
installation fails closed with `componentUnavailable` instead of pretending
that source archives are installed models. Verify, path, and remove return the
same error for this source-only entry and ignore forged install-state directories.
Format conversion, OCR inference,
provider requests, and plugin execution remain unavailable.

The detailed design documents are maintained in Chinese. See the
[architecture](docs/architecture.md), [interface contract](docs/interfaces.md),
[format matrix](docs/formats.md), [OCR and AI design](docs/ocr-and-ai.md),
[security model](docs/security.md), and [testing strategy](docs/testing.md).
The authoritative CLI and configuration specifications are maintained in
Chinese: [command-line design](docs/cli.md) and
[configuration](docs/configuration.md). License policy, third-party sources,
and release auditing are covered by [license governance](docs/licensing.md).
