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
They are implemented but not yet executed: the coordinating task is serializing
local build and real-sample windows. Formatting and diff-whitespace checks pass.
Real acceptance will compare OCR-off/auto for the reported ODP, including native
content, original asset identities, OCR contribution and located diagnostics.
