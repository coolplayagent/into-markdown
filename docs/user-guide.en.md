# User installation, offline deployment, and troubleshooting

[中文](user-guide.md) · [CLI examples](cli-examples.en.md)

A release contains one platform Core, two self-contained capability plugins, and the Agent Skill.
Every Core/plugin has SHA-256, signature, SPDX, source, and notice sidecars. Combine only artifacts
from the same version and target.

| Capability | Artifact |
| --- | --- |
| Ordinary documents, PDF, and Web workbench | Platform Core |
| OCR | `official.ocr.ppocrv6.imp` |
| Transcription and diarization | `official.media.whisper.imp` |
| Agent instructions | `into-markdown-skill.zip` |

Legacy `.doc/.ppt/.xls` files are not shipped in the current release and never invoke LibreOffice;
[#191](https://github.com/coolplayagent/into-markdown/issues/191) tracks the replacement parser path.

## Install Core

On macOS ARM64, verify digest, notarization, and mounted content before running the DMG installer:

```sh
shasum -a 256 -c into-md-macos-arm64-core.dmg.sha256
spctl --assess --type open --verbose=2 into-md-macos-arm64-core.dmg
hdiutil attach into-md-macos-arm64-core.dmg
cd "/Volumes/Into Markdown"
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

Use the volume name printed by `hdiutil`; macOS x86_64 is unsupported.

On Linux, select the x86_64 or ARM64 archive matching `uname -m`:

```sh
sha256sum -c into-md-linux-x86_64-core.tar.gz.sha256
gpg --verify into-md-linux-x86_64-core.tar.gz.asc into-md-linux-x86_64-core.tar.gz
mkdir into-md-core
tar -xzf into-md-linux-x86_64-core.tar.gz -C into-md-core
cd into-md-core
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

Use `into-md-linux-arm64-core.tar.gz` on ARM64. The installer never edits shell profiles.

On Windows x86_64, verify the ZIP digest and Authenticode of project executables inside it:

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64-core.zip).Hash
Expand-Archive .\into-md-windows-x86_64-core.zip .\into-md-core
Get-AuthenticodeSignature .\into-md-core\bin\into-md.exe | Format-List
& .\into-md-core\bin\archive-check.exe .\into-md-core
& .\into-md-core\Install.ps1
```

The digest must match the release sidecar and Authenticode `Status` must be `Valid`.

Repeating the same Linux or Windows install verifies and repairs that version instead of returning
a conflict. An upgrade keeps the immutable old version and switches authority only after the new
archive passes every check. On Windows, the `into-md.exe` on PATH is a stable launcher: do not add a
`versions/<digest>/bin` directory to PATH or edit the adjacent `into-md.prefix`. If an in-use file
blocks upgrade or removal, stop the identified local task and retry; the failed operation preserves
the prior installation.

## Verify and install capabilities

```sh
into-md version --json
into-md formats --json
into-md capabilities list --json
into-md doctor --json
into-md setup ocr
into-md setup media
```

`setup` uses the network only for that explicit installation. Conversion and status commands never
download plugins or models; models remain internal to complete plugins.

## Complete offline deployment

Verify Core, both `.imp` files, and sidecars on a connected machine, then transfer them through
controlled media. Install Core and use its pinned official publisher identity:

```sh
installed="$HOME/.local/share/into-markdown/current"
catalog="$installed/share/into-markdown/plugins/official-publisher.json"
signer_id=$(jq -r .signingKeyId "$catalog")
signer_sha=$(jq -r .signingKeySha256 "$catalog")
for package in official.ocr.ppocrv6 official.media.whisper; do
  file="/media/release/$package.imp"
  sha=$(sha256sum "$file" | awk '{print $1}')
  into-md plugins install "$file" --sha256 "$sha" \
    --signing-key-id "$signer_id" --signing-key-sha256 "$signer_sha" --scope global
  into-md plugins verify "$package" --scope global
done
into-md capabilities list --json
```

Use `shasum -a 256` on macOS and `Get-FileHash` plus the same catalog fields on Windows. Do not add
`--allow-network` during offline installation.

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

```sh
./uninstall "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

```powershell
& .\into-md-core\Uninstall.ps1
```

The uninstaller removes only the product tree and command shim, never a user-copied or linked Agent Skill.
