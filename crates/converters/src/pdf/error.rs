use super::{ConversionError, PdfiumError};

pub(super) fn map_pdfium_error(error: PdfiumError) -> ConversionError {
    match error {
        PdfiumError::Native { operation: "load_document", code: 4 | 5 } => {
            ConversionError::Encrypted
        }
        PdfiumError::Native { operation: "load_document", code } => {
            malformed("document", format!("PDFium rejected the PDF (native error {code})"))
        }
        PdfiumError::ResourceLimit { limit: "pdfium_runtime_bytes", .. } => {
            ConversionError::ComponentUnavailable {
                component: "pdfium".into(),
                detail: error.to_string(),
            }
        }
        PdfiumError::ResourceLimit { limit, actual, maximum } => {
            resource(limit, format!("{actual} > {maximum}"))
        }
        PdfiumError::InvalidPath(_)
        | PdfiumError::DigestMismatch { .. }
        | PdfiumError::BinaryValidation(_)
        | PdfiumError::Load(_)
        | PdfiumError::UnsupportedPlatform { .. } => ConversionError::ComponentUnavailable {
            component: "pdfium".into(),
            detail: error.to_string(),
        },
        PdfiumError::InvalidResult { operation, detail } => malformed(operation, detail),
        PdfiumError::Allocation { operation, bytes } => {
            resource("max_memory_bytes", format!("{operation} could not allocate {bytes} bytes"))
        }
        PdfiumError::Poisoned => ConversionError::Internal { detail: error.to_string() },
        PdfiumError::Native { operation, code } => {
            malformed(operation, format!("PDFium native error {code}"))
        }
    }
}

pub(super) fn malformed(part: impl Into<String>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

pub(super) fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}
