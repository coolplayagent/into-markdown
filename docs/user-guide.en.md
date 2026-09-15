# User installation, offline deployment, and troubleshooting

[中文](user-guide.md) · [CLI examples](cli-examples.en.md)

A release provides Core ZIPs for four platforms, matching optional speech plugins, an Agent Skill, and an audit ZIP. Core includes the OCR models and runtime. Use artifacts from the same version and target platform. `UNSIGNED` means operating-system publisher signatures are absent; speech `.imp` packages retain internal Ed25519 manifest signatures.

| Platform | Core |
| --- | --- |
| macOS Apple Silicon | `into-md-macos-arm64.zip` |
| Linux x86_64 | `into-md-linux-x86_64.zip` |
| Linux ARM64 | `into-md-linux-arm64.zip` |
| Windows x86_64 | `into-md-windows-x86_64.zip` |

Core handles documents, Office 97–2003, PDF, OCR, and the Web workbench. Transcription and diarization use `official.media.whisper-<target>.imp`; Agent instructions use `into-markdown-skill.zip`. macOS x86_64 is unsupported.

## Install Core

Download the matching ZIP from the [release page](https://github.com/coolplayagent/into-markdown/releases) and verify its SHA-256 against the release asset. The audit ZIP contains the release inventory, sources, licenses, and build evidence. Core also includes license materials.

macOS example:

```sh
shasum -a 256 into-md-macos-arm64.zip
unzip into-md-macos-arm64.zip -d into-md-core
./into-md-core/into-md version --json
```

On Linux, select the ZIP matching `uname -m`, verify it with `sha256sum`, extract it, and run `./into-md-core/into-md`. Add the extracted directory to PATH to invoke `into-md` directly.

Windows PowerShell example:

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64.zip).Hash
Expand-Archive .\into-md-windows-x86_64.zip .\into-md-core
& .\into-md-core\into-md.exe version --json
```

Keep the Windows directory intact, including its bundled PDFium files. Unsigned artifacts may trigger operating-system trust prompts; verify the source and digest before allowing execution. Unsigned macOS artifacts have ad-hoc signatures needed for execution, without Developer ID notarization.

For upgrades, extract to a new directory and finish old conversion processes before switching the command path. Configuration and installed speech plugins remain in the user data directory. Keep the previous directory if rollback is needed.

## Verify and install capabilities

```sh
into-md version --json
into-md formats --json
into-md capabilities list --json
into-md doctor --json
into-md setup media
```

Core includes OCR and defaults to `best-effort` with `auto` OCR. `setup media` explicitly downloads and installs the speech plugin. Ordinary conversion uses installed capabilities.

## Offline deployment

Download and verify Core on a connected machine. If speech is needed, also obtain the `.imp` matching the version and target platform, then transfer both to the offline machine. Extracting Core makes document conversion and OCR available.

Run `into-md ui` and import the local speech `.imp` through plugin management. Core's embedded catalog checks official package digests and publisher identity. Verify installation with `into-md capabilities list --json`. See [plugin management](plugin-management.md) for CLI offline import and publisher fingerprint arguments.

## Conversion and networking

```sh
into-md report.docx -o report.md --conflict error --log-format json
into-md documents --recursive --output-dir markdown --conflict error --dry-run
into-md documents --recursive --output-dir markdown --conflict error \
  --report conversion-report.json --log-format json
into-md meeting.webm --ai audio-transcription=only --diarize \
  -o meeting.md --conflict error --log-format json
```

Remote input needs per-invocation `--allow-network`; narrow it with `--allow-host` and separately
authorize loopback/private targets. See [CLI examples](cli-examples.en.md).

## Troubleshoot and uninstall

Preserve the exit status and stable `--log-format json` event, then run `into-md doctor --json`.

| Signal | Action |
| --- | --- |
| `componentUnavailable` | Use `capabilities show <ID> --json`, then run `setup` or reinstall offline. |
| `networkDenied` | Confirm remote intent, authorize the exact host, and authorize private networking separately. |
| `outputConflict` | Preserve the old file and overwrite only with explicit authority. |
| `malformed` / `invalidMedia` | The input is damaged or mismatched; do not rename it. |
| `pluginSandboxUnavailable` | Check Core/plugin targets and platform isolation. |
| `hashMismatch` / `invalidManifest` | Stop and obtain a verified official artifact. |

Use `doctor --deep` only when ordinary diagnostics cannot locate damage. Do not expose API keys,
query-bearing URLs, private paths, or sensitive content in public issues.

To uninstall portable Core, remove the directory you extracted and any PATH entry you added. Keep or remove user configuration, installed plugins, conversion outputs, and separately copied Agent Skills as needed.
