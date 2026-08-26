# User installation, offline deployment, and troubleshooting

[中文](user-guide.md) · [CLI examples](cli-examples.en.md)

A release contains one platform Core, two self-contained capability plugins, and the Agent Skill.
Every Core/plugin has SHA-256, SPDX, source, and notice sidecars. Each target's
`*-signing-policy.json` states whether an external publisher signature is present. The default release
mode is `unsigned`: it is installable, but the operating system cannot verify the publisher identity.
Both `.imp` files always retain internal Ed25519 manifest signatures and pinned SHA-256 values.

| Capability | Artifact |
| --- | --- |
| Ordinary documents, Office 97–2003, PDF, and Web workbench | Platform Core |
| OCR | `official.ocr.ppocrv6-<target>.imp` |
| Transcription and diarization | `official.media.whisper-<target>.imp` |
| Agent instructions | `into-markdown-skill.zip` |

Core natively parses Office 97–2003 `.doc/.ppt/.xls` files.

## Install Core

On macOS ARM64, verify the digest, then mount the DMG according to its signing policy:

```sh
shasum -a 256 -c into-md-macos-arm64-core.dmg.sha256
# For unsigned releases only, remove quarantine after the digest matches.
xattr -d com.apple.quarantine into-md-macos-arm64-core.dmg 2>/dev/null || true
hdiutil attach into-md-macos-arm64-core.dmg
cd "/Volumes/into-markdown" # use the actual path printed by hdiutil
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

Unsigned DMGs use ad-hoc Mach-O signatures for Apple silicon execution, but have no Developer ID or
Apple notarization. Alternatively, keep quarantine and choose Open Anyway in Privacy & Security.
Only a `signed` policy should pass `spctl --assess --type open --verbose=2`. macOS x86_64 is unsupported.

On Linux, select the x86_64 or ARM64 archive matching `uname -m`:

```sh
sha256sum -c into-md-linux-x86_64-core.tar.gz.sha256
mkdir into-md-core
tar -xzf into-md-linux-x86_64-core.tar.gz -C into-md-core
cd into-md-core
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

Use `into-md-linux-arm64-core.tar.gz` on ARM64. A `.asc` is present only for a `signed` policy; verify
it with GPG in that mode. For unsigned releases, the SHA-256 sidecar adjacent to the GitHub Release
asset is the pre-install authority. The installer never edits shell profiles.

On Windows x86_64, verify the ZIP digest first. For an unsigned ZIP, remove its download mark only
after that digest matches:

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64-core.zip).Hash
Unblock-File .\into-md-windows-x86_64-core.zip
Expand-Archive .\into-md-windows-x86_64-core.zip .\into-md-core
& .\into-md-core\bin\archive-check.exe .\into-md-core
powershell -NoProfile -ExecutionPolicy Bypass -File .\into-md-core\Install.ps1
```

The digest must match the release sidecar. Unknown publisher and SmartScreen warnings are expected
for unsigned releases; never bypass them if the digest differs. Only a `signed` policy should be
checked with `Get-AuthenticodeSignature`, whose `Status` must then be `Valid`.

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

`setup` is the explicit networked command for installing a complete capability plugin, including
its models and runtime. Conversion and status commands use the currently installed capability state.

## Complete offline deployment

Verify Core, both `.imp` files, and sidecars on a connected machine, then transfer them through
controlled media. Install Core and use its pinned official publisher identity:

```sh
installed="$HOME/.local/share/into-markdown/current"
catalog="$installed/share/into-markdown/plugins/official-publisher.json"
signer_id=$(jq -r .signingKeyId "$catalog")
signer_sha=$(jq -r .signingKeySha256 "$catalog")
target=x86_64-unknown-linux-gnu # replace with the current platform target
for package in official.ocr.ppocrv6 official.media.whisper; do
  file="/media/release/$package-$target.imp"
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

The uninstaller manages the product tree and command shim. Users separately manage any copied or
linked Agent Skill directory.
