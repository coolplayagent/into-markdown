# Third-party notices

This repository is licensed under Apache-2.0. The dependency inventory and
release gate are maintained under `third_party/licenses/`.

## Current distribution boundary

The Rust packages in `Cargo.lock` are build dependencies of the CLI and
libraries. `third_party/licenses/rust-lock.tsv` records an exact, reviewed
license choice for every non-workspace package. A distributor must preserve
each package's copyright and license text; the registry source for every
package is fixed by `Cargo.lock`.

ONNX Runtime 1.29.0 archives and the PP-OCRv6 source archives are hash-pinned,
manual inputs. They are not linked into or copied into normal build outputs.
Their sources and declared licenses are recorded in their existing manifests.

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

The audit rejects included components that are planned, unknown, incomplete,
denied, or absent from the inventory. Packaging must copy `LICENSE`, `NOTICE`,
this file, and the applicable upstream license and notice texts into the
archive. The audit establishes inventory completeness; it does not replace
legal review or synthesize upstream copyright notices.
