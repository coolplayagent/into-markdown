# Plugin package management

`into-md plugins` installs signed `process-v1` and `wasi-v1` packages into an explicit project or
global store. A package is a bounded ZIP containing `plugin.json` plus the exact regular-file
inventory covered by an Ed25519 signature. The signed payload binds the schema, publisher key ID
and fingerprint, plugin ID/version/protocol, target entrypoints, runtime manifest, file sizes and
SHA-256 digests. The source ZIP SHA-256 remains a separate transfer pin.

Global publisher trust lives only in the owner-private global plugin store. Project configuration
may reference that authority but cannot create or widen it. Revocation is enforced by both key ID
and public-key fingerprint, and one fingerprint cannot be aliased under multiple IDs.

```text
into-md plugins install ./converter.zip --sha256 <zip-sha256> \
  --signing-key-id example.publisher --signing-key-sha256 <public-key-sha256> --scope global
into-md plugins verify converter.id --scope global
into-md plugins disable converter.id --scope global
into-md plugins enable converter.id --scope global
into-md plugins run converter.id input.bin --input-format application/example --scope global
into-md plugins remove converter.id --scope global
```

HTTPS installation requires an explicit source SHA-256 and uses the audited HTTP transport. The
response is identity encoded, streamed to a private temporary file, bounded before DNS/connect,
and rechecked by the package manager. Redirects are disabled and private addresses remain denied.

Installation and removal use a store lock, durable phase journal, private staging, verified rename
publication and restart recovery. Verification rejects missing or additional files, links,
reparse points, hard links, portable path aliases, unsafe modes, package/receipt mutation, target
or protocol drift, and any mismatch with the exact scope configuration pin.

Execution is available only for an enabled, verified package whose scope config pins the exact
source hash, protocol, publisher key ID and globally trusted fingerprint. `process-v1` runs through
the process sandbox with an empty environment. `wasi-v1` runs through Wasmtime with an empty
invocation capability set; package installation never grants network, clock, random or filesystem
authority.

The normal test gates are:

```text
cargo test --locked -p into-markdown-plugin-manager -j1
cargo test --locked -p into-markdown-http-transport -j1
cargo test --locked -p into-markdown-cli -j1
bazel test //crates/plugin-manager:plugin_manager_test //crates/plugin-manager:plugin_manager_process_e2e_test --jobs=1
```

## Package schema and signature bytes

`plugin.json` is UTF-8 JSON with no unknown fields. Its complete schema is:

```json
{
  "schemaVersion": 1,
  "id": "publisher.converter",
  "version": "1.2.3",
  "protocol": "process-v1",
  "supportedTargets": ["x86_64-pc-windows-msvc"],
  "entrypoints": {"x86_64-pc-windows-msvc": "bin/converter.exe"},
  "runtimeManifest": null,
  "files": [{"path": "bin/converter.exe", "bytes": 1234, "sha256": "<64 lowercase hex>", "executable": true}],
  "signature": {
    "signedPayloadVersion": 1,
    "algorithm": "ed25519",
    "keyId": "publisher.release",
    "publicKeyBase64": "<32-byte key, standard base64>",
    "publicKeySha256": "<64 lowercase hex>",
    "signedPayloadSha256": "<64 lowercase hex>",
    "signatureBase64": "<64-byte signature, standard base64>"
  }
}
```

For `wasi-v1`, `runtimeManifest` is the inventoried WASI runtime-manifest path. For
`process-v1` it must be null. The supported target set and entrypoint-map keys must be identical
and may contain only `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`.

The Ed25519 input is the compact UTF-8 JSON emitted by `serde_json::to_vec` for these fields in
this exact order:

```text
signatureDomain, signedPayloadVersion, algorithm, keyId, publicKeySha256,
schemaVersion, id, version, protocol, supportedTargets, entrypoints,
runtimeManifest, files
```

`signatureDomain` is exactly `into-markdown/plugin-package/v1`. Sets and maps serialize in bytewise
sorted key order; `files` must already be sorted by `path`. Hash those exact bytes into
`signedPayloadSha256`, then sign the same bytes with Ed25519. The source ZIP digest passed to
`--sha256` is separate and is never substituted for the signed content identity.

The repository-provided signer builds those bytes and a deterministic stored ZIP directly:

```text
cargo run --locked -p into-markdown-plugin-manager --bin package_plugin -- \
  SOURCE_DIR manifest-template.json ed25519-key.pkcs8 publisher.release plugin.zip
```

The template contains the top-level fields through `runtimeManifest`; the tool inventories and
hashes `SOURCE_DIR`, derives the public key and fingerprint, signs the canonical payload, and writes
the complete `plugin.json`. The signed `executable` bit is derived from each source file's Unix
execute permission; installation strips execute permission from every file not carrying that
authority and verifies the resulting tree. This covers declared entrypoints and authenticated
helper processes without trusting ZIP mode metadata. The signer refuses links, special files,
`plugin.json` in the source tree,
non-portable paths, oversized files, and an existing output.

## ZIP and path rules

The archive is at most 256 MiB, its central directory at most 8 MiB, and it contains exactly one
`plugin.json` plus 1–4096 declared regular files. A file is at most 128 MiB and the manifest at
most 1 MiB. ZIP64, multidisk archives, links, reparse points, hard links after installation,
special modes, duplicate entries, undeclared entries, and decompression-ratio bombs are rejected.

Paths are ASCII portable relative paths using `/`. Total length is at most 1024 bytes and each
segment at most 240 bytes. Empty, `.`, `..`, repeated separators, backslashes, absolute/drive/UNC
paths, control/space/colon characters, trailing dot/space, case-fold aliases, Windows device names
(`CON`, `PRN`, `AUX`, `NUL`, `CLOCK$`, `COM1`–`COM9`, `LPT1`–`LPT9`), and manager-reserved names are
rejected. Each segment contains only ASCII letters, digits, `.`, `_`, and `-`.

## Scope identity and recovery

Global packages live below the protected per-user application-data anchor. Project packages do
not live in the project tree: their store is
`project-plugins/<SHA-256(domain || filesystem identity || canonical path bytes)>` under that same
protected anchor. Unix path bytes are used verbatim; Windows uses the canonical UTF-16LE units plus
volume and file ID. Moving or recreating a project therefore creates a different scope. Pending
journals remain isolated in the old store and can be garbage-collected only after that old identity
is no longer active; they never authorize writes into the replacement project.

Portable deployments and isolated integration tests may set `INTO_MARKDOWN_USER_DATA_HOME` to an
existing absolute, canonical, owner-private directory. This single authority relocates both the
global configuration to `into-markdown/config.toml` and the plugin stores beneath
`into-markdown/`; it never combines a relocated store with the operating-system configuration
directory. Links/reparse points, non-owner Unix modes, and unauthorized Windows DACL entries are
rejected before either location is read. `--no-config` bypasses configuration and recovery and is
therefore accepted only for the empty `plugins` list/show view; install, verify, enable, disable,
run, and remove reject that combination.

Configuration replacement and the plugin store/config/trust operation are journaled separately and
recovered, under their locks, before the first plugins/doctor configuration read. On Windows the
implementation flushes ordinary files and provides tested process-crash recovery. It deliberately
does not claim power-loss directory-fsync semantics on volumes that reject directory
`FlushFileBuffers`.

Publisher trust publication has an explicit indeterminate state. If publication is incomplete but
an authenticated `.trusted-signers.next` is durable, the CLI retains the signed store/config/trust
forward intent and reports the install location without rolling back to a state that could later
widen trust by itself. The next `plugins` command or `doctor` acquires the same locks, authenticates
the pending file and journal, finishes publication, and removes authenticated temporary state
before loading configuration.
