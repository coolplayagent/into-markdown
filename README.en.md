# Into Markdown

[中文](README.md) · [Documentation map](docs/README.md)

Into Markdown is a local-first, offline-by-default document-to-Markdown product. It combines a
Rust conversion core, the `into-md` CLI, a local Web workbench, stable JSON and Bundle contracts,
and two self-contained optional capability plugins. Every result passes through the
provenance-aware Document IR and the single deterministic GFM renderer.

The project is independent of the neighbouring `anydoc` and `markitdown` projects and does not
wrap either implementation.

## Product composition

- Core: CLI, Web workbench, Document IR, detection and conversion, PDFium, secure output
  transactions, plugin and provider administration, and installed-package acceptance tools.
- OCR plugin `official.ocr.ppocrv6`: PP-OCRv6, ONNX Runtime, worker, models, and character table.
- Speech plugin `official.media.whisper`: FFmpeg, Whisper, VAD, diarization models, and runtimes.
- Agent Skill `into-markdown`: portable instructions for compatible agents, released as a
  standalone ZIP and embedded byte-for-byte in every Core package.

Models are private implementation resources of complete capability plugins. They are not separate
product objects to install, update, or switch. Ordinary conversion never downloads plugins,
models, or remote content.

## Supported surface

The current format catalog includes PDF, DOCX, PPTX, XLSX, ODT/ODS/ODP, RTF, EPUB,
text, Markdown, HTML, CSV/TSV, JSON, XML, RSS/Atom feeds, Jupyter notebooks, images, ZIP, Outlook
MSG, audio, and video. OCR, transcription, and diarization use their corresponding capability
plugins. Legacy `.doc/.ppt/.xls` files are not shipped in the current release and never fall back
to LibreOffice; [#191](https://github.com/coolplayagent/into-markdown/issues/191) tracks their
replacement parser path.

Release targets are macOS ARM64, Linux x86_64, Linux ARM64, and Windows x86_64. macOS x86_64 is
unsupported. See the [installation and deployment guide](docs/user-guide.en.md) for signature
verification, installation, offline plugin import, troubleshooting, and uninstall. Platform release
contracts are the [macOS release](docs/macos-arm64-release.md) and
[Linux/Windows release](docs/platform-modular-release.md).

## CLI

Conversion accepts inputs directly; there is no `convert` subcommand:

```sh
into-md version --json
into-md report.docx -o report.md --conflict error --log-format json
into-md scan.png --ocr always -o scan.md --conflict error --log-format json
into-md meeting.webm --ai audio-transcription=only \
  -o meeting.md --conflict error --log-format json
```

Plan batch work before conversion, then inspect every report item:

```sh
into-md documents/ --recursive --output-dir markdown/ \
  --conflict error --dry-run --log-format json
into-md documents/ --recursive --output-dir markdown/ \
  --conflict error --report conversion-report.json --log-format json
```

Remote sources and providers are denied by default. Add `--allow-network` only when the current
user request requires it, narrow access with `--allow-host` where possible, and require separate
`--allow-private-network` authority for private destinations.

See the [CLI](docs/cli.md), [executable command and format examples](docs/cli-examples.en.md),
[configuration](docs/configuration.md), [format matrix](docs/formats.md), and [DTOs](docs/dto.md).

## Capabilities and local Web workbench

```sh
into-md capabilities list --json
into-md capabilities show ocr --json
into-md setup ocr
into-md setup media
into-md doctor --json
into-md ui
```

Each `setup` command installs and verifies a complete official capability plugin; conversion never
invokes setup implicitly. `into-md ui` listens only on `127.0.0.1` and provides batch conversion,
progress and cancellation, task history, artifact preview/download, and format, capability,
provider, plugin, configuration, and diagnostic administration. CLI and Web paths share the same
routing, configuration, and security boundaries.

See [capability plugins](docs/capability-plugins.md),
[plugin management](docs/plugin-management.md), and the [local Web service](docs/ui.md).

## Agent Skill

The standalone `into-markdown-skill.zip` can be explicitly extracted into an agent's discovery
directory. Every Core also includes `share/into-markdown/skills/into-markdown/` for users who prefer
to copy or link the canonical directory. Product installers and uninstallers never mutate agent
directories. Codex can invoke `$into-markdown` explicitly or select it for matching conversion
requests.

See the [Agent Skill release and installation guide](docs/agent-skill.md).

## Development and verification

```sh
bazel build //...
bazel test //...
cargo check --workspace
```

Bazel is authoritative for release builds; Cargo supports fast development checks and focused
tests. Release claims require black-box testing from a freshly installed Core with real documents,
images, and media. Compilation, mocks, and skill-structure validation are not substitutes.

The [documentation map](docs/README.md) groups the architecture, security, testing, and release
contracts. Licensing and supply-chain evidence are in [LICENSE](LICENSE),
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), and [license governance](docs/licensing.md).
Read the [contribution guide](CONTRIBUTING.en.md) before changing the repository and the
[plugin-development guide](docs/plugin-development.en.md) before extending converters or capability
providers.
