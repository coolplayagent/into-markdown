# macOS ARM64 archives

The macOS release adapter produces two independently audited archives for Apple silicon:

- `core` contains `into-md`, PDFium, ONNX Runtime, the default detector/recognizer/dictionary, the legacy worker, the standalone Rust package, and installed-smoke fixtures. Modern Office, OpenDocument, PDF, and OCR work without a system package manager.
- `full` contains the same files plus the pinned private LibreOffice runtime. It supports `.doc`, `.ppt`, and `.xls` without installing an application into `/Applications`.

Both profiles target macOS 14 or newer, matching the minimum deployment target of the pinned ONNX Runtime library. They do not read Homebrew libraries or development-tree absolute paths. Ordinary Cargo and Bazel builds do not download release assets; only the explicit release adapter reads the fixed HTTPS URL, byte count, and SHA-256 authority in `tools/macos-release/authority.json`.

## Build

Run on a native ARM64 Mac with Rust 1.97.1. Use an empty output and build directory for every reproducibility sample:

```sh
PYTHONPATH=tools/macos-release python3 tools/macos-release/release.py \
  --profile core \
  --output /private/tmp/into-md-core-stage \
  --cache /private/tmp/into-md-release-cache \
  --build-root /private/tmp/into-md-core-build \
  --archive /private/tmp/into-md-core.tar.gz
```

Replace `core` with `full` and use distinct paths for the full archive. A release is accepted only when two clean runs of the same profile have identical archive SHA-256 values.

## Install and remove

Extract the archive into a new directory, then run its installer. Neither command needs administrator privileges:

```sh
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
./uninstall "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

Installation is content-addressed, repeatable, and switches `current` only after the complete tree is copied. An interrupted upgrade leaves the previous `current` target intact. The uninstaller removes only exact 64-character lowercase SHA-256 version directories owned by this installer.

## Verification

The archive root contains `archive-manifest.json`, `sbom-input.json`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, and the exact license/source materials selected by its profile. Validation must be run from a newly extracted archive and then from the installed `current` directory. The installed smoke contract covers CLI and offline external Rust consumption, modern Office/OpenDocument formats, PDF, OCR, malformed input, and ZIP. Core additionally requires the exact missing-legacy-runtime result; full additionally converts representative DOC, PPT, and XLS inputs.
