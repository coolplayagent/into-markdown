//! Last-resort delivery for readable sources whose native parser could not finish.
use into_markdown_core::{
    Asset, AssetId, AssetMode, Block, BlockNode, ConversionError, ConversionOptions,
    ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ErrorPolicy, ExecutionContext,
    Inline, InputFormat, NodeId, Provenance, ProvenanceKind, ResolvedInput, SourceLocator,
};
use sha2::{Digest, Sha256};

pub(crate) fn recoverable(options: &ConversionOptions, error: &ConversionError) -> bool {
    options.error_policy == ErrorPolicy::BestEffort
        && options.output.asset_mode != AssetMode::Omit
        && (matches!(
            error,
            ConversionError::Malformed { .. }
                | ConversionError::Unsupported { .. }
                | ConversionError::EmptyContent
                | ConversionError::ArchiveExtractionRequired { .. }
        ) || (options.ai.image_description != into_markdown_core::AiMode::Only
            && matches!(error, ConversionError::ComponentUnavailable { .. })))
}

pub(crate) async fn detect_for_conversion(
    detectors: &[std::sync::Arc<dyn into_markdown_core::FormatDetector>],
    input: &ResolvedInput,
    hint: &into_markdown_core::FormatHint,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<into_markdown_core::FormatCandidate>, ConversionError> {
    let error = match crate::nested::detect_formats(detectors, input, hint, context).await {
        Ok(candidates) if !candidates.is_empty() => return Ok(candidates),
        Ok(_) => ConversionError::Unsupported {
            detail: "Format detectors found no supported source identity".into(),
        },
        Err(error) => error,
    };
    if !recoverable(options, &error) {
        return Err(error);
    }
    let filename = hint.filename.as_deref().or(input.metadata.name.as_deref());
    let extension = hint
        .extension
        .as_deref()
        .or_else(|| filename.and_then(|name| name.rsplit_once('.').map(|(_, suffix)| suffix)));
    let Some(format) = hint.format.or_else(|| extension.and_then(InputFormat::from_extension))
    else {
        return Err(error);
    };
    let mut candidate = into_markdown_core::FormatCandidate::new(format, 0.0, error.to_string());
    candidate.detector_id = "builtin.source-recovery".into();
    Ok(vec![candidate])
}

pub(crate) fn retain_empty_source(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
    output: ConverterOutput,
    format: InputFormat,
) -> Result<ConverterOutput, ConversionError> {
    if !input.bytes.is_empty()
        && output.source_content_evidence() == into_markdown_core::SourceContentEvidence::Unknown
        && output.document.metadata.title.as_deref().is_none_or(|title| title.trim().is_empty())
        && output.assets.is_empty()
        && into_markdown_core::document_is_empty(&output.document)
        && recoverable(options, &ConversionError::EmptyContent)
    {
        drop(output);
        converter_failure(input, options, context, ConversionError::EmptyContent, format)
    } else {
        Ok(output)
    }
}

pub(crate) fn converter_failure(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
    error: ConversionError,
    format: InputFormat,
) -> Result<ConverterOutput, ConversionError> {
    if !recoverable(options, &error) {
        return Err(error);
    }
    context.checkpoint()?;
    let bytes = input.bytes.len() as u64;
    if bytes > options.limits.max_asset_bytes || bytes > options.limits.max_total_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: "original source recovery exceeds the requested asset budget".into(),
        });
    }
    let reason = format!(
        "Native conversion could not finish. The complete original file is attached for recovery. {error}"
    );
    let lease = context
        .reserve_memory(bytes.saturating_add(reason.len() as u64 * 2).saturating_add(65536))?;
    let id = format!("source-{:x}", Sha256::digest(&input.bytes));
    let extension = input
        .metadata
        .name
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension)
        .filter(|extension| {
            extension.len() <= 16 && extension.bytes().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or("bin");
    let mut payload = Vec::new();
    payload.try_reserve_exact(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "original source recovery allocation failed".into(),
    })?;
    payload.extend_from_slice(&input.bytes);
    let locator =
        SourceLocator { byte_start: Some(0), byte_end: Some(bytes), ..SourceLocator::default() };
    let provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "builtin.source-recovery".into(),
        locator: locator.clone(),
        confidence: None,
    };
    ConverterOutput::new_with_memory_reservations(
        Document {
            blocks: vec![
                BlockNode {
                    id: NodeId("source-recovery-reason".into()),
                    provenance: provenance.clone(),
                    block: Block::Paragraph(vec![Inline::Text {
                        value: reason.clone(),
                        marks: Vec::new(),
                    }]),
                },
                BlockNode {
                    id: NodeId("source-recovery-original".into()),
                    provenance,
                    block: Block::Image {
                        asset: AssetId(id.clone()),
                        alt: Some("Original source — complete file".into()),
                    },
                },
            ],
            ..Document::default()
        },
        vec![Asset {
            id: AssetId(id.clone()),
            filename: Some(format!("{id}.{extension}")),
            media_type: original_media_type(format, extension).into(),
            bytes: payload,
            external_uri: None,
        }],
        vec![Diagnostic {
            code: "conversion.recovery.originalFile".into(),
            severity: DiagnosticSeverity::Warning,
            message: reason,
            locator: Some(locator),
        }],
        vec![lease],
    )
    .account_retained(context)
}

fn original_media_type(format: InputFormat, extension: &str) -> &'static str {
    match format {
        InputFormat::Pdf => "application/pdf",
        InputFormat::Doc => "application/msword",
        InputFormat::Docx => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        InputFormat::Ppt => "application/vnd.ms-powerpoint",
        InputFormat::Pptx => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        InputFormat::Xls => "application/vnd.ms-excel",
        InputFormat::Xlsx if extension.eq_ignore_ascii_case("xlsb") => {
            "application/vnd.ms-excel.sheet.binary.macroenabled.12"
        }
        InputFormat::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        InputFormat::Odt => "application/vnd.oasis.opendocument.text",
        InputFormat::Ods => "application/vnd.oasis.opendocument.spreadsheet",
        InputFormat::Odp => "application/vnd.oasis.opendocument.presentation",
        InputFormat::Rtf => "application/rtf",
        InputFormat::Epub => "application/epub+zip",
        InputFormat::Text => "text/plain",
        InputFormat::Markdown => "text/markdown",
        InputFormat::Html => "text/html",
        InputFormat::Csv => "text/csv",
        InputFormat::Tsv => "text/tab-separated-values",
        InputFormat::Json => "application/json",
        InputFormat::Xml | InputFormat::Feed => "application/xml",
        InputFormat::Drawio => "application/vnd.jgraph.mxfile",
        InputFormat::Ipynb => "application/x-ipynb+json",
        InputFormat::Zip => "application/zip",
        InputFormat::Rar => "application/vnd.rar",
        InputFormat::OutlookMsg => "application/vnd.ms-outlook",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        ConversionOutcome, ExecutionOptions, SourceMetadata, conversion_outcome,
    };
    use std::sync::Arc;

    fn source() -> ResolvedInput {
        ResolvedInput {
            bytes: Arc::from(&b"readable source bytes"[..]),
            metadata: SourceMetadata {
                name: Some("broken.docx".into()),
                ..SourceMetadata::default()
            },
        }
    }
    #[test]
    fn unsupported_detection_recovers_only_under_best_effort() {
        use std::future::Future;
        fn run<F: Future>(future: F) -> F::Output {
            let mut future = std::pin::pin!(future);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                    return value;
                }
            }
        }
        let mut options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let input = source();
        let hint = into_markdown_core::FormatHint::default();
        let candidates =
            run(detect_for_conversion(&[], &input, &hint, &options, &context)).unwrap();
        assert_eq!(candidates[0].format, InputFormat::Docx);
        let attempt = run(crate::preparation::select_converter(
            &[],
            &input,
            &candidates,
            &options,
            &context,
            &[],
        ))
        .unwrap();
        let output = converter_failure(
            &input,
            &options,
            &context,
            attempt.probe_error.unwrap(),
            attempt.candidate.format,
        )
        .unwrap();
        assert_eq!(output.assets[0].bytes, input.bytes.as_ref());
        options.error_policy = ErrorPolicy::Strict;
        assert!(matches!(
            run(detect_for_conversion(&[], &input, &hint, &options, &context)),
            Err(ConversionError::Unsupported { .. })
        ));
    }

    fn malformed() -> ConversionError {
        ConversionError::Malformed {
            part: Some("document/body".into()),
            detail: "incomplete structure".into(),
        }
    }
    #[test]
    fn parser_failure_preserves_original_hash_and_releases_its_lease() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let input = source();
        let output =
            converter_failure(&input, &options, &context, malformed(), InputFormat::Docx).unwrap();
        assert_eq!(output.assets[0].bytes.as_slice(), input.bytes.as_ref());
        assert_eq!(
            output.assets[0].media_type,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(output.assets[0].id.0, format!("source-{:x}", Sha256::digest(&input.bytes)));
        assert_eq!(conversion_outcome(&output.diagnostics), ConversionOutcome::Degraded);
        output.document.validate().unwrap();
        assert!(context.reserved_memory_bytes() > 0);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
    #[test]
    fn empty_native_document_retains_source_and_strict_output_is_unchanged() {
        let mut options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let output = retain_empty_source(
            &source(),
            &options,
            &context,
            ConverterOutput::default(),
            InputFormat::Docx,
        )
        .unwrap();
        assert_eq!(output.assets[0].bytes, source().bytes.as_ref());
        assert_eq!(conversion_outcome(&output.diagnostics), ConversionOutcome::Degraded);
        options.error_policy = ErrorPolicy::Strict;
        let strict = retain_empty_source(
            &source(),
            &options,
            &context,
            ConverterOutput::default(),
            InputFormat::Docx,
        )
        .unwrap();
        assert!(strict.assets.is_empty());
        assert!(strict.document.blocks.is_empty());
    }

    #[test]
    fn unavailable_media_decoder_delivers_original_with_reason() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error = ConversionError::ComponentUnavailable {
            component: "builtin.asr.whisper-small".into(),
            detail: "decoder rejected truncated media".into(),
        };
        let output =
            converter_failure(&source(), &options, &context, error, InputFormat::Audio).unwrap();
        assert_eq!(output.assets[0].bytes, source().bytes.as_ref());
        assert!(output.diagnostics[0].message.contains("decoder rejected truncated media"));
    }

    #[test]
    fn recovery_preserves_strict_omission_and_resource_errors() {
        let mut options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        options.error_policy = ErrorPolicy::Strict;
        assert!(matches!(
            converter_failure(&source(), &options, &context, malformed(), InputFormat::Docx),
            Err(ConversionError::Malformed { .. })
        ));
        options.error_policy = ErrorPolicy::BestEffort;
        options.output.asset_mode = AssetMode::Omit;
        assert!(matches!(
            converter_failure(&source(), &options, &context, malformed(), InputFormat::Docx),
            Err(ConversionError::Malformed { .. })
        ));
        options.output.asset_mode = AssetMode::Extract;
        let limited = ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "explicit budget".into(),
        };
        assert!(matches!(
            converter_failure(&source(), &options, &context, limited, InputFormat::Docx),
            Err(ConversionError::ResourceLimit { .. })
        ));
        options.limits.max_asset_bytes = 1;
        assert!(matches!(
            converter_failure(&source(), &options, &context, malformed(), InputFormat::Docx),
            Err(ConversionError::ResourceLimit { .. })
        ));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}
