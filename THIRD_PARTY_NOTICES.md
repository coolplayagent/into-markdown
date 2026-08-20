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

`calamine 0.36.1` parses XLSX/XLSM/XLSB only after the repository-owned
OPC/BIFF12 scanner authenticates the third-party reading surface and its
combined retained/transient peak under the engine's request credit. The panic
boundary is error mapping, not allocator isolation. Calamine is distributed
under MIT; Copyright (c) 2016 Johann Tuffe. The complete upstream license is
preserved in `third_party/licenses/calamine-MIT.txt`. The reviewed crate
checksum is `5fa68281b1a76b54a62156474adb06bb380a67e07dd60656e3217152b42183f3`
and its recorded upstream VCS revision is
`0a24c2a9f1e38c0932c1299e633270dc730db505`.

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
`third_party/licenses/npm/react-MIT.txt`. Lucide React icons are distributed
under ISC, with the listed Feather-derived icons retaining their MIT notice;
the complete combined upstream text is preserved at
`third_party/licenses/npm/lucide-ISC-MIT.txt`. Both texts must accompany
distributions.
`third_party/licenses/npm-release.spdx.json` describes the exact content-hashed
production JavaScript and its four runtime packages. The release audit verifies
that SPDX graph, the checked asset bytes, the inventory, and the upstream license
files in both directions. The SBOM and both complete license texts are mandatory members
of the root `//:release_license_files` distribution set. Build and test
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

The optional meeting pipeline acquires its models only through the explicit
`setup media` / `models install` flow; ordinary builds, tests, and conversions
never download them. Silero VAD v6.2.1 is MIT-licensed, Copyright (c)
2020-present Silero Team, with the complete text preserved in
`third_party/licenses/silero-vad-MIT.txt`. The 3D-Speaker ERes2Net model is
Apache-2.0; the complete license text is the repository `LICENSE`. Both ONNX
files are bound to exact HTTPS authorities, sizes, and SHA-256 values in
`models/manifest.json` and `third_party/licenses/downloads.json`. Neither model
is copied into the current release archives.

PDFium `153.0.7999.0` (`chromium/7999`) is reviewed and hash-pinned for four
platforms, but remains a manual input and is not in current release outputs. Its
redistribution archives include the upstream BSD license and a `licenses/` directory
for bundled permissive dependencies; both must be preserved if distributed.

FFmpeg, LibreOffice, Wasmtime, generated models, and distribution fonts remain
machine-readable `planned` entries. No version, build configuration, source, or
compliance conclusion is asserted for them. In particular, a future FFmpeg entry must document a reproducible
LGPL-compatible configuration and prove that GPL/nonfree components are off.

Noto Sans CJK SC Regular is a hash-pinned, manual fixture-generator input under
OFL-1.1. The complete license text is in `third_party/licenses/OFL-1.1.txt`.
The font itself is not committed or included in release outputs; repository-authored
text rendered into checked-in OCR PNG fixtures is distributed under Apache-2.0.
The PP-OCRv6 tiny recognizer ONNX archive is separately hash-pinned under
Apache-2.0 for an explicit OCR quality target and is likewise absent from ordinary
build and release outputs. Their exact source, size, hash, and boundary are audited
across `fixtures/manifest.json`, the inventory, and the dedicated
`fixtures/downloads.json` authority.

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

Wasmtime 39.0.1 and its WASI Preview 2 support crates are source-built Rust
dependencies for the optional plugin runtime. Their crates.io checksums,
upstream tag and commit, and exact feature policy are fixed in
`third_party/wasmtime/source.json`. They are licensed under Apache-2.0 WITH
LLVM-exception; the complete upstream text is preserved at
`third_party/licenses/wasmtime-Apache-2.0-LLVM-exception.txt`. The current CLI
release graph does not include this runtime until plugin registration is wired.

The audit rejects Cargo dependency drift, unmanaged source-less packages,
managed manifest/inventory/download drift, and tracked release components that
are planned, unknown, incomplete, or denied. Packaging must copy `LICENSE`,
`NOTICE`, this file, and the applicable upstream license and notice texts into
the archive. The audit does not inspect a completed archive, prove that every
file in it was inventoried, replace legal review, or synthesize upstream
copyright notices.
