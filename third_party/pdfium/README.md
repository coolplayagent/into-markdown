# PDFium runtime

The boundary pins the non-V8 `bblanchon/pdfium-binaries` release `chromium/7999`
(`153.0.7999.0`). This reproducible build packages upstream PDFium headers, its BSD license, and
the complete permissive third-party license directory. It is a redistribution, not a claim that
Google publishes official binary SDK archives.

Normal Cargo and Bazel builds are offline with respect to PDFium. Runtime repositories and native
smoke checks are `manual`; fetching is an explicit operator action. `manifest.json` is the audit
record for archive hashes, extracted-library hashes, platform formats, imports, and the exact C ABI
exports used by the Rust crate.

`Pdfium::load_pinned()` accepts only an absolute non-symlink path named for the current supported
target. It requires the exact reviewed file size, performs a fallible size-plus-one bounded read,
hashes and parses those bytes, and writes them into a private read-only snapshot. The loader maps
that retained snapshot file descriptor rather than the caller-controlled path, so concurrent
in-place writes and path replacement cannot change the verified bytes. Format, architecture,
64-bit class, imports, and every consumed export are checked before mapping; unsupported targets
fail closed.

All native calls are serialized. Document bytes remain alive until `FPDF_CloseDocument`; Rust
borrows force text/page/image handles to close before their parent. Input, password, page, text,
image, dimension, pixel, bitmap-byte, and allocation sizes are hard-checked before native traversal
or allocation. `Image::bitmap()` returns a bounded owned copy with explicit dimensions, stride, and
pixel format; it never exposes PDFium-owned memory.
PDFium still parses untrusted native input in-process, so applications with hostile PDFs should add
an OS sandbox/process boundary; this crate does not claim memory isolation.

Explicit artifact/static ABI audit (networked, about 15 MB):

```sh
PDFIUM_AUDIT_NETWORK=1 ./tools/pdfium-audit.sh
```

Native smoke on macOS ARM64 (downloads the matching artifact, validates it through the production
loader, and extracts exact text plus a real embedded image before rendering the generated PDF):

```sh
PDFIUM_NATIVE_SMOKE=1 PDFIUM_AUDIT_NETWORK=1 ./tools/pdfium-audit.sh --native-smoke
```
