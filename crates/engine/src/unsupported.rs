//! Shared terminal diagnostics for recognized, unsupported containers.
use into_markdown_core::{
    ConversionError, FormatCandidate, InputFormat, RarSignature, ResolvedInput,
};

pub(crate) fn check(
    input: &ResolvedInput,
    candidates: &[FormatCandidate],
) -> Result<(), ConversionError> {
    if candidates.is_empty() {
        return Err(ConversionError::Unsupported {
            detail: "format detectors produced no candidates".into(),
        });
    }
    match RarSignature::detect(&input.bytes) {
        Some(RarSignature::Rar4 | RarSignature::Rar5) => Err(ConversionError::Unsupported {
            detail: "RAR archive conversion is unsupported; extract the archive first, then convert the extracted files. RAR 归档请先解压后再转换。".into(),
        }),
        Some(RarSignature::Damaged) => Err(ConversionError::Malformed {
            part: input.metadata.name.clone(),
            detail: "RAR signature is truncated or invalid; obtain a complete archive, then extract it before conversion".into(),
        }),
        None if candidates.first().is_some_and(|candidate| candidate.format == InputFormat::Rar) => Err(ConversionError::Malformed {
            part: input.metadata.name.clone(),
            detail: "input is labelled RAR but has no complete RAR4/5 signature; check the file contents".into(),
        }),
        None => Ok(()),
    }
}
