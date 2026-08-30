# Issue #324: embedded JPEG OCR boundary

The standalone JPEG wrapper continues to require its first structural EOI to
end exactly at EOF, regardless of error policy. Embedded visual OCR uses the
same marker scanner, but in best-effort mode may borrow the JPEG codestream
through that EOI and exclude trailing bytes from the OCR input. Strict embedded
mode rejects trailing bytes. There is no new public option or trusted-source
marker, and no filename or trailing-byte-content exception.

This adaptation is not pixel validation. The existing bounded image decoder
must still decode the complete selected codestream before PNG normalization and
OCR. Malformed structure, invalid pixels, resource limits, cancellation and
timeouts continue to propagate. No original asset bytes or identities change.

After successful complete decoding, `embeddedVisualOcr.jpegTrailingData` reports
the excluded byte count and explicitly states that the original asset is
unchanged. It receives the existing image-reference locator and remains present
if automatic OCR has no available provider. No compatibility diagnostic is
issued as a substitute for a failed pixel decode.

Five targeted tests cover pixel/asset preservation and deduplicated OCR calls,
strict/standalone rejection, marker-payload boundaries and malformed pixels,
resource/cancellation/timeout propagation, and provider-unavailability diagnostics.
All five pass on implementation `0bf2adef2aef2cc6444054c0c85145c56190d2f6`.
Formatting and diff-whitespace checks pass, as do all four existing PR CI jobs.

## Real ODP acceptance

The frozen manifest sample `8b16843138c57e6fd8163158` has source SHA-256
`1fbb574d53faa5b7f5a9ddc139ce770441b321536d57ba03edb196ff9aed7de4`.
Its original classification remains unchanged (`unclassified`). The debug CLI
was built with `embedded-runtime`, borrowing the exact ten OCR payload files
from the coordinating task's authenticated a6d4 candidate cache. Per-file SHA
checks prove that provider, worker and model bytes were not replaced. The frozen
OCR configuration and environment were reused, and capability inspection
confirmed ready `core:ocr` 0.0.4 before conversion.

Both serialized OCR-off and OCR-auto runs succeeded. With `asset-mode omit`,
Markdown grew from 13,658 to 15,316 UTF-8 bytes. Result JSON retained original
assets for verification: all seven are byte-identical between modes and their
SHA-256 identities match their original source ZIP members. Removing only the
154 newly added OCR nodes from auto reproduces the complete off Document exactly,
including all 368 native nodes and their order/provenance. The added OCR text
contains 1,308 characters, matching CLI contribution telemetry.

Six compatibility diagnostics each report 17 trailing bytes and locate their
images on slides 14, 16, 30, 32, 35 and 37. Existing layout and low-confidence
diagnostics remain visible; successful conversion is not a claim of perfect OCR
transcription. Strict rejection is covered by the targeted fixtures, not by an
additional real-source run.

Evidence is under `C:/im-bench-004/issue324-jpeg-20260831`: `tests.log`, `build.log`,
`identity.json`, `payload-identity.json`, `capability.json`, `runs.json`, the two
reports/result artifacts, `summary.json` and `ocr-text.txt`. `run.py` performs the
two conversions; `verify.py` compares existing outputs without another run.
The executable SHA-256 is
`dc3b35ecb383bd65a9ec19ba01e88a1a49a19faed84c0b876fe57530d57e0040`.
Observed debug process times were 1.657 s off and 12.502 s auto; these are
correctness-run observations, not a release performance gate or regression score.
