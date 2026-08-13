# Third-party notices

This repository is licensed under Apache-2.0. The dependency inventory and
release gate are maintained under `third_party/licenses/`.

## Current distribution boundary

The Rust packages in `Cargo.lock` are build dependencies of the CLI and
libraries. `third_party/licenses/rust-lock.tsv` records an exact, reviewed
SPDX obligation conclusion for every non-workspace package. Every term joined
by `AND` applies; in particular, `unicode-ident` is concluded as
`MIT AND Unicode-3.0`. A distributor must preserve
each package's copyright and license text; the registry source for every
package is fixed by `Cargo.lock`.

ONNX Runtime 1.29.0 archives and the PP-OCRv6 source archives are hash-pinned,
manual inputs. They are not linked into or copied into normal build outputs.
Their sources and declared licenses are recorded in their existing manifests.
The audit and the local Bzlmod extension share
`third_party/licenses/downloads.json`, so every managed artifact's URL and hash
has one structured download source of truth.

PDFium `153.0.7999.0` (`chromium/7999`) is reviewed and hash-pinned for four
platforms, but remains a manual input and is not in current release outputs. Its
redistribution archives include the upstream BSD license and a `licenses/` directory
for bundled permissive dependencies; both must be preserved if distributed.

FFmpeg, LibreOffice, Wasmtime, generated models, and fonts remain
machine-readable `planned` entries. No version, build configuration, source, or
compliance conclusion is asserted for them. In particular, a future FFmpeg entry must document a reproducible
LGPL-compatible configuration and prove that GPL/nonfree components are off.

## Release obligation

Before creating an archive, update the inventory to describe every included
component and run:

```shell
bazel run //tools/license-check:release_audit
```

The audit rejects Cargo dependency drift, unmanaged source-less packages,
managed manifest/inventory/download drift, and tracked release components that
are planned, unknown, incomplete, or denied. Packaging must copy `LICENSE`,
`NOTICE`, this file, and the applicable upstream license and notice texts into
the archive. The audit does not inspect a completed archive, prove that every
file in it was inventoried, replace legal review, or synthesize upstream
copyright notices.
