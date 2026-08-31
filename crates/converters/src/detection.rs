//! Bounded signature, container and text evidence for shared routing.
use super::{
    ZIP_INSPECTION_ENTRY_LIMIT, ZIP_NAME_READ_LIMIT, admission, drawio, inspect_zip_package,
    structured_text_candidate, text, zip_preflight,
};
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, FormatCandidate, FormatDetector, FormatHint,
    InputFormat, ResolvedInput,
};
use std::io::Cursor;

/// Detector for file signatures and bounded inspection of ZIP/OLE containers.
#[derive(Debug, Default)]
pub struct ContentFormatDetector;

impl FormatDetector for ContentFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.content"
    }

    fn priority(&self) -> i32 {
        200
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            let result = admission::detect(input, hint, context)?;
            if let Some(detail) = result.unsupported_reason {
                return Err(ConversionError::Unsupported { detail });
            }
            Ok(result.candidates)
        })
    }

    fn detect_with_authority<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<into_markdown_core::FormatDetection, ConversionError>> {
        Box::pin(async move { admission::detect(input, hint, context) })
    }
}

#[cfg(test)]
pub(super) fn detect_content(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Vec<FormatCandidate>, ConversionError> {
    if let Some(candidates) = admission::binary_candidates(bytes, &mut Vec::new()) {
        return Ok(candidates);
    }
    detect_text(bytes, context, &mut into_markdown_core::DetectionAuthority::Heuristic)
}

pub(super) fn detect_text(
    bytes: &[u8],
    context: &ExecutionContext,
    authority: &mut into_markdown_core::DetectionAuthority,
) -> Result<Vec<FormatCandidate>, ConversionError> {
    if drawio::evidence(bytes, context)? {
        *authority = into_markdown_core::DetectionAuthority::StructuredText;
        return Ok(vec![FormatCandidate::new(InputFormat::Drawio, 0.99, "Drawio graph root")]);
    }
    if let Some(candidate) = structured_text_candidate(bytes, context, authority)? {
        return Ok(vec![candidate]);
    }
    Ok(text::sniff_unstructured_text(bytes, context)?
        .map(|confidence| {
            FormatCandidate::new(
                InputFormat::Text,
                confidence,
                "plain-text safety and encoding thresholds",
            )
        })
        .into_iter()
        .collect())
}

#[cfg(test)]
pub(super) fn detect_zip(bytes: &[u8]) -> Vec<FormatCandidate> {
    detect_zip_with_hints(bytes, &mut Vec::new())
}

pub(super) fn detect_zip_with_hints(
    bytes: &[u8],
    compatible_hints: &mut Vec<InputFormat>,
) -> Vec<FormatCandidate> {
    *compatible_hints = admission::zip_package_hints();
    let mut candidates = vec![FormatCandidate::new(InputFormat::Zip, 0.90, "ZIP magic bytes")];
    let entry_count = match zip_preflight(bytes) {
        Ok(entry_count) if entry_count <= ZIP_INSPECTION_ENTRY_LIMIT => entry_count,
        Ok(entry_count) => {
            candidates[0].diagnostics.push(format!(
                "ZIP inspection stopped before archive construction: {entry_count} entries exceed the {ZIP_INSPECTION_ENTRY_LIMIT} entry limit"
            ));
            return candidates;
        }
        Err(diagnostic) => {
            candidates[0].diagnostics.push(diagnostic);
            return candidates;
        }
    };
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(error) => {
            candidates[0]
                .diagnostics
                .push(format!("ZIP directory could not be inspected: {error}"));
            return candidates;
        }
    };
    if archive.len() != entry_count {
        candidates[0].diagnostics.push(format!(
            "ZIP entry count changed after validated EOCD preflight: {entry_count} != {}",
            archive.len()
        ));
        return candidates;
    }

    let mut names = Vec::with_capacity(archive.len());
    let mut name_bytes = 0_usize;
    let mut complete_names = true;
    for index in 0..archive.len() {
        match archive.by_index(index) {
            Ok(entry) => {
                name_bytes = name_bytes.saturating_add(entry.name().len());
                if name_bytes > ZIP_NAME_READ_LIMIT {
                    candidates[0].diagnostics.push(format!(
                        "ZIP inspection stopped: entry names exceed the {ZIP_NAME_READ_LIMIT} byte limit"
                    ));
                    return candidates;
                }
                names.push(entry.name().replace('\\', "/"));
            }
            Err(error) => {
                complete_names = false;
                candidates[0]
                    .diagnostics
                    .push(format!("ZIP entry {index} could not be inspected: {error}"));
            }
        }
    }
    // An empty container has no member identity; retain labelled-package validation.
    if complete_names && !names.is_empty() {
        compatible_hints.retain(|format| admission::zip_package_hint_matches(*format, &names));
    }
    let specialized = inspect_zip_package(&mut archive, &names, &mut candidates[0].diagnostics);
    if specialized.len() == 1 {
        let (format, evidence) = specialized[0];
        candidates.push(FormatCandidate::new(format, 0.99, evidence));
    } else if specialized.len() > 1 {
        candidates[0].diagnostics.push(format!(
            "conflicting package structures detected: {}",
            specialized.iter().map(|(format, _)| format.as_str()).collect::<Vec<_>>().join(",")
        ));
    }
    candidates
}
