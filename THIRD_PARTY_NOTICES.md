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

`clipper2-rust 1.1.0` is used for closed-polygon round offsets in OCR text
detection. It is distributed under BSL-1.0; the complete license text is in
`third_party/licenses/BSL-1.0.txt`. The reviewed crates.io source checksum is
`0fd663fe209e7030c956e3be4c051dcc20cdb73da794f31466762cff12ca11bf`, and its
recorded upstream VCS revision is `09e9505f99a18136505a64485011a292d4375a3a`.
The reviewed source is a 18,590-line pure-Rust port with `forbid(unsafe_code)`,
no build script, and only `num-traits` as a runtime dependency. Any upgrade
requires a new audit.

The request-accounted Suzuki-Abe contour scanner in
`crates/ocr/src/detection.rs` is adapted from `imageproc 0.25.0`
`src/contours.rs`, Copyright (c) 2015 PistonDevelopers, under the MIT License.
The crates.io source checksum is
`2393fb7808960751a52e8a154f67e7dd3f8a2ef9bd80d1553078a7b4e8ed3f0d`.
The complete MIT permission and warranty text is in
`third_party/licenses/imageproc-MIT.txt`.
The integer line-mask compatibility routine follows the Apache-2.0 OpenCV
4.13 `LineIterator` algorithm; OpenCV is used only as documented reference
source and is not linked, downloaded, or shipped by ordinary builds.

The local task store pins `rusqlite 0.37.0` and `libsqlite3-sys 0.35.0` under
MIT and builds the crate's bundled SQLite 3.50.2 amalgamation instead of a
system library. SQLite's amalgamation is dedicated to the public domain by
its authors. Conditional upstream build helpers remain lock- and
license-inventoried even where the bundled build does not execute them.

The embedded Web console is built from the integrity-pinned `pnpm-lock.yaml`.
`third_party/licenses/npm-inventory.json` covers every locked npm package and
distinguishes runtime, build, and test scope. React, React DOM, and Scheduler
are MIT-licensed runtime code included in the minified console asset; their MIT
copyright and complete license text are preserved at
`third_party/licenses/npm/react-MIT.txt` and must accompany distributions.
`third_party/licenses/npm-release.spdx.json` describes the exact content-hashed
production JavaScript and its three runtime packages. The release audit verifies
that SPDX graph, the checked asset bytes, the inventory, and the upstream license
file in both directions. Build and test
packages are not copied into the CLI release. `axe-core` is MPL-2.0 and is used
only by the accessibility test: its source package and CI/cache artifacts retain
the upstream MPL file-level notices and source availability obligations, while
none of its code enters the generated console assets.

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

FFmpeg 8.1.2 is an optional, separately built LGPL-2.1-or-later runtime. The
project does not ship an upstream or system binary. Its signed source authority,
minimal LGPL-only configuration, and distribution obligations are documented in
`third_party/ffmpeg/`; generated binaries must retain the upstream LGPL notices
and corresponding-source/relinking rights.
```

The audit rejects Cargo dependency drift, unmanaged source-less packages,
managed manifest/inventory/download drift, and tracked release components that
are planned, unknown, incomplete, or denied. Packaging must copy `LICENSE`,
`NOTICE`, this file, and the applicable upstream license and notice texts into
the archive. The audit does not inspect a completed archive, prove that every
file in it was inventoried, replace legal review, or synthesize upstream
copyright notices.
