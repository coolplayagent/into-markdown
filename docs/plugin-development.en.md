# Plugin development

Into Markdown accepts only plugins that are signed, explicitly installed, and executed inside an
isolation boundary. A plugin cannot bypass the common Document IR, resource validation, output
transaction, or per-invocation network authority. User-facing OCR and speech capabilities ship as
complete `.imp` packages containing their runtimes, models, licenses, and SBOMs, with the complete
plugin as the management unit. Core natively provides Office 97–2003 parsing.

## Choose a protocol

| Protocol | Use it for | Default authority | Result boundary |
| --- | --- | --- | --- |
| `process-v1` | Converters and capability providers that need native libraries, authenticated helpers, or platform runtimes | Empty environment, no network, and only request-private input and temporary storage | Length-prefixed JSON, stable events, and a `ResultDto` or typed capability DTO |
| `wasi-v1` | Portable converters that compile to a WASI Preview 2 command component | Files, clocks, random, and network are all denied until individually granted | Bounded JSON envelope, common IR, and resource inventory |

See [`process-plugins.md`](process-plugins.md) and [`wasi-plugins.md`](wasi-plugins.md) for the wire
protocols and sandbox boundaries. Capability providers must also follow the identity, readiness,
DTO, and routing rules in [`capability-plugins.md`](capability-plugins.md).

## Develop and verify `process-v1`

The entrypoint implements the handshake, one request, strictly increasing events, and one terminal
response. It must not interpret shell commands, read the inherited environment, or accept arbitrary
host paths. Start with the repository fixture and the real manager end-to-end gate:

```sh
cargo test --locked -p into-markdown-process-plugin
bazel test //crates/process-plugin:process_plugin_test
bazel test //crates/plugin-manager:plugin_manager_process_e2e_test
```

A capability provider's `provider.json`, entrypoint, helpers, fixed models, dictionaries, licenses,
and SBOM all enter the signed `plugin.json` file inventory. Results must pass the public DTO
validators; a plugin cannot emit unvalidated Markdown instead of IR.

## Develop and verify `wasi-v1`

The component implements `wasi:cli/run@0.2.x` and its manifest pins `wasiPreview`, the Wasmtime
version, component SHA-256, and supported host targets. Rebuild the checked-in fixture before
running the real component:

```sh
python crates/plugin-wasi/tests/verify_fixture.py --rebuild
cargo test --locked -p into-markdown-plugin-wasi --test runtime -j1
bazel test //crates/plugin-wasi:plugin_wasi_runtime_test --jobs=1 --local_resources=memory=4096
```

Grant a preopen, clock, random source, or exact IP/port network destination only when the capability
requires it. A private destination also requires a separate `allowPrivate` grant. The host still
validates paths, resources, IR, and provenance at execution time.

## Build a signed package

The source directory contains only required regular runtime files. `manifest-template.json`
declares the package ID, version, protocol, supported targets, entrypoints, and optional runtime
manifest. Never commit the release private key.

```sh
openssl genpkey -algorithm Ed25519 -outform DER -out developer-ed25519.pk8
cargo run --locked -p into-markdown-plugin-manager --bin package_plugin -- \
  plugin-root manifest-template.json developer-ed25519.pk8 developer.example example.imp
```

The packager sorts files deterministically and rejects links, special files, unsafe paths, existing
output, and undeclared content before signing the complete inventory. The transfer SHA-256 of the
`.imp` file and the signing public-key fingerprint are independent pins; record both in a release.
See [`plugin-management.md`](plugin-management.md) for the complete schema and signed bytes.

## Accept the complete lifecycle

Use an isolated user-data directory and real input to verify install through removal:

```sh
into-md plugins install ./example.imp --sha256 <PACKAGE_SHA256> \
  --signing-key-id developer.example --signing-key-sha256 <PUBLIC_KEY_SHA256> --scope global
into-md plugins verify <PLUGIN_ID> --scope global --json
into-md plugins enable <PLUGIN_ID> --scope global
into-md plugins run <PLUGIN_ID> sample.bin --input-format application/example --scope global
into-md plugins disable <PLUGIN_ID> --scope global
into-md plugins remove <PLUGIN_ID> --scope global
```

Cover invalid signatures, added or changed files, target mismatch, unavailable capabilities,
cancellation, timeouts, resource limits, default network denial, and explicit grants. Native
plugins must execute a real binary on every declared platform; cross-compilation is not runtime
evidence. Release packages include third-party licenses, SBOM, source and runtime inventories, and
must pass the repository license, plugin-manager, installed-smoke, and archive-check gates.
