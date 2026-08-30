//! Container JPEG adaptation before the existing bounded pixel decoder.

use crate::image_converter::envelope;
use into_markdown_core::{
    ConversionError, ConversionOptions, Diagnostic, DiagnosticSeverity, ErrorPolicy,
    ExecutionContext,
};

pub(super) fn envelope<'a>(
    bytes: &'a [u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(envelope::Summary, &'a [u8]), ConversionError> {
    let end = envelope::jpeg_codestream_end(bytes, &options.limits, context)?;
    if end != bytes.len() && options.error_policy == ErrorPolicy::Strict {
        return Err(ConversionError::Malformed {
            part: Some("image".into()),
            detail: "embedded JPEG has trailing bytes after EOI in strict mode".into(),
        });
    }
    if options.limits.max_pages == 0 {
        return Err(super::resource("max_pages", "1 image frame exceeds the request limit"));
    }
    // Borrow only the structurally delimited codestream. The caller must still
    // decode it completely; this neither repairs pixels nor changes the asset.
    Ok((envelope::Summary { frames: 1, animated: false }, &bytes[..end]))
}

pub(super) fn diagnostic(trailing_bytes: usize) -> Option<Diagnostic> {
    (trailing_bytes != 0).then(|| Diagnostic {
        code: "embeddedVisualOcr.jpegTrailingData".into(),
        severity: DiagnosticSeverity::Warning,
        message: format!(
            "Ignored {trailing_bytes} trailing byte(s) after JPEG EOI for OCR after complete pixel decoding; original asset bytes are unchanged"
        ),
        locator: None,
    })
}
