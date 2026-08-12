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
Their sources and declared licenses are recorded in their existing manifests,
and the audit binds every managed artifact's URL and hash to `MODULE.bazel`.

PDFium, FFmpeg, LibreOffice, Wasmtime, generated models, and fonts are not in
current release outputs. They remain machine-readable `planned` entries. No
version, build configuration, source, or compliance conclusion is asserted for
them. In particular, a future FFmpeg entry must document a reproducible
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
