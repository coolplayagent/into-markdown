# into-markdown

[中文](README.md)

`into-markdown` is a Rust-first document-to-Markdown conversion platform built
with Bazel. The repository currently contains the architecture, public service
provider interfaces, registry and pipeline shell, command-line shell, and
contract tests. It does not yet contain production format parsers, OCR
inference, network clients, or LLM calls.

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
bazel run //apps/cli:into-md -- doctor
```

The CLI accepts conversion inputs directly and has no `convert` subcommand. It
defines batch files and directories, stdin, URIs, OCR/AI routing, structured
JSON, portable bundles, layered configuration, providers, models, and plugins.
Network and AI access are disabled by default; remote sources and providers
require an explicit `--allow-network` on every invocation.

Format conversion, OCR inference, model installation, provider requests, and
plugin execution backends are not implemented yet. Their commands return stable
errors and never report a false success.

The detailed design documents are maintained in Chinese. See the
[architecture](docs/architecture.md), [interface contract](docs/interfaces.md),
[format matrix](docs/formats.md), [OCR and AI design](docs/ocr-and-ai.md),
[security model](docs/security.md), and [testing strategy](docs/testing.md).
The authoritative CLI and configuration specifications are maintained in
Chinese: [command-line design](docs/cli.md) and
[configuration](docs/configuration.md). License policy, third-party sources,
and release auditing are covered by [license governance](docs/licensing.md).
