# License governance architecture

This document defines the repository-wide authority for source, dependency, model, font,
native-runtime, download, and release licensing. The policy is platform neutral: macOS, Linux,
and Windows packaging may select different component sets, but may not reach different license
conclusions for the same component.

## Scope and ownership

The project is distributed under Apache License 2.0. `LICENSE` is the complete project license,
`NOTICE` is the project attribution, and `THIRD_PARTY_NOTICES.md` explains the reviewed third-party
obligations. These files are mandatory inputs to every source or binary distribution.

`third_party/licenses/inventory.json` is the component authority. A component has one stable ID,
one reviewed license conclusion, one exact upstream source and version, explicit redistribution
obligations, and a release status. Platform packaging issues consume this authority; they do not
copy or reinterpret it.

The following files are projections or evidence and must agree with the component authority:

- `Cargo.lock` and `third_party/licenses/rust-lock.tsv` describe the complete Rust dependency set.
- `pnpm-lock.yaml`, `third_party/licenses/npm-inventory.json`, checked-in license texts, and the npm
  SPDX document describe console assets shipped by Bazel.
- `models/manifest.json` and model authority files describe source archives, derived runtime files,
  character tables, exact members, sizes, hashes, licenses, and supported targets.
- Native runtime manifests describe ONNX Runtime and PDFium archives. FFmpeg source and fixture
  manifests provide the fixed upstream source and build evidence.
- `third_party/licenses/downloads.json` and its Bazel projection bind every controlled download to
  the same component ID, URL, target, size where known, SHA-256, and extraction boundary.
- `fixtures/manifest.json` and `fixtures/downloads.json` distinguish repository-owned test data from
  manually acquired licensed inputs. Fixtures are not silently promoted to release components.

LibreOffice, Wasmtime, generated models, and distribution fonts remain denied from release while
their inventory entries are planned or lack complete immutable acquisition and notice evidence.

## Trust boundaries and threat model

The audit treats all manifests, lockfiles, Bazel labels, archive entry lists, and user-supplied
release projections as untrusted input. It runs offline and does not attempt to repair, infer, or
download missing evidence.

Threats addressed by the audit include:

- an unknown dependency or native binary entering a build without a reviewed component;
- a known component being substituted through a changed URL, version, size, hash, archive member,
  target, dynamic dependency, or Bazel repository declaration;
- an allowed license string hiding a denied, unknown, compound, or unreviewed conclusion;
- a model, font, or fixture being distributed from a source-only or manual-only declaration;
- Cargo, npm, Bazel, model, fixture, native-runtime, and download declarations drifting in either
  direction;
- an FFmpeg binary enabling GPL, nonfree, external-library, networking, or autodetected features;
- a platform archive omitting required declarations or carrying an orphan file/component;
- an SBOM describing components that are absent from an archive, or omitting components that are
  present;
- path traversal, duplicate IDs, duplicate archive paths, Unicode-confusable identifiers, and
  non-canonical URLs creating ambiguous authority;
- a platform pipeline weakening a license conclusion by maintaining a separate allowlist.

The audit fails closed. Unknown schema fields, malformed or duplicate records, missing declarations,
missing hashes, unbound downloads, unsupported targets, incompatible licenses, planned components,
and inconsistent projections are errors. A successful audit is evidence that the checked metadata
is internally consistent; it is not a substitute for legal review when adding or upgrading a
component.

## Supply-chain workflow

Adding or upgrading a component requires one review transaction:

1. Assign a stable component ID and record its exact upstream source, version, license conclusion,
   obligations, and release status in the component authority.
2. Record immutable acquisition evidence. Remote bytes require a canonical HTTPS URL, SHA-256,
   size when the upstream format exposes one, safe extraction paths, and an exact target mapping.
3. Record ecosystem evidence: Cargo and npm lock entries, Bazel repositories/labels, model members,
   native dependencies, or fixture provenance as applicable.
4. Preserve required license and notice texts in the repository and bind them to the component.
5. For FFmpeg, preserve the source signature/checksum and configuration evidence proving the build
   disables GPL, version3, nonfree, external libraries, networking, and autodetection. A generic
   `LGPL` label without build evidence is insufficient.
6. Run the offline repository audit and strict release audit. Both compare declarations in both
   directions, so an unused authority record and an undeclared build input are independently
   rejected where the schema requires a complete set.
7. Review the generated NOTICE and SBOM inputs. Generated output is derived from stable IDs and
   sorted deterministically; generated files never become an alternate policy authority.

Ordinary builds and audits do not use the network. A controlled acquisition step may download only
an authority record that has already passed schema and policy validation, then verifies size and
SHA-256 before exposing bytes to extraction or packaging.

## Release projection contract

Packaging for issues #133, #134, and #135 supplies a projection of the archive it actually built.
The projection is data, not policy. It contains:

- a supported platform target;
- the exact set of component IDs present in the archive;
- every archived file path and SHA-256, with each non-project file owned by exactly one component;
- paths to the project `LICENSE`, `NOTICE`, generated third-party notices, and SBOM input;
- the FFmpeg build-evidence digest when FFmpeg is present.

The license checker exposes a narrow archive-verification operation that accepts this projection
and returns success or a sorted list of policy violations. It does not create archives, install
toolchains, download dependencies, run installation smoke tests, or infer components from platform
names. Those responsibilities belong to the platform delivery issues.

Verification applies these invariants:

- each projected component exists, is reviewed, is release-eligible, and has a complete license,
  source, version, and obligations declaration;
- every component has at least one owned archive entry and every non-project archive entry has one
  known owner;
- mandatory declaration paths exist and their hashes match repository-generated inputs;
- the SBOM and NOTICE inputs contain exactly the projected third-party component set;
- native and model files match their target-specific authority, hashes, and download bindings;
- FFmpeg is accepted only with matching LGPL-compatible build evidence;
- changing only the platform target cannot change a component's license conclusion.

This API intentionally verifies metadata and hashes supplied by a packaging implementation. Archive
materialization, deterministic tar/zip behavior, installation, and smoke testing remain outside this
issue.

### Command-line adapter

The narrow API is also exposed as an offline command:

```text
cargo run -p license-check --bin release-projection -- generate REQUEST.json
cargo run -p license-check --bin release-projection -- verify ARCHIVE-PROJECTION.json
```

`generate` accepts `schema_version`, one supported Rust target triple, and a sorted component ID
list. Inventory components keep their stable IDs; crates use `cargo:name@version`, and shipped npm
packages use `npm:name@version`. Its JSON output contains the exact bytes, sizes, and SHA-256 values
for `NOTICE`, generated `THIRD_PARTY_NOTICES.md`, and `sbom-input.json`.

`verify` accepts the same target and component set plus archive files. A component file names its
single component owner. A project binary may use `embedded_components` to bind the Cargo/npm/source
components compiled into that binary without pretending they are separate archive files. Required
declarations and generated metadata have no component owner. All paths are normalized ASCII relative
paths; all file hashes are lowercase SHA-256 values.

The checked-in files under `tools/license-check/fixtures/` exercise identical FFmpeg conclusions for
all four supported targets. They are contract fixtures, not platform policy copies.

## Review and maintenance

Policy changes require independent severity-based review. P0 findings cover incompatible licensing,
unknown or orphan release content, missing mandatory declarations, and bypasses of fail-closed
behavior. P1 findings cover incomplete provenance, one-way consistency, target confusion, and
non-deterministic notice or SBOM inputs. P2 findings cover maintainability and diagnostics that do
not weaken a release decision.

The checker is split by authority domain (`schema`, `rust`, `npm`, `models_fixtures`, `native`,
`release`, and `sbom`) so ownership remains visible and no platform-specific policy fork can grow
inside a general-purpose module.
