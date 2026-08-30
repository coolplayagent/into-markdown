//! Engine-side source evidence attachment and terminal result validation.

use into_markdown_core::{
    ASSET_ONLY_REASON_CODE, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    EMPTY_SOURCE_REASON_CODE, ExecutionContext, InputFormat, ResolvedInput, SourceContentEvidence,
    document_is_asset_only, document_is_empty,
};

const EVIDENCE_DIAGNOSTIC_PEAK_BYTES: u64 = 512;

pub(crate) fn attach_evidence(
    mut output: ConverterOutput,
    source: &ResolvedInput,
    format: InputFormat,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if output.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic.code.as_str(), EMPTY_SOURCE_REASON_CODE | ASSET_ONLY_REASON_CODE)
    }) {
        return Err(ConversionError::Internal {
            detail: "converter emitted an engine-reserved result diagnostic".into(),
        });
    }
    let evidence = match output.source_content_evidence() {
        SourceContentEvidence::Unknown
            if format == InputFormat::Markdown
                && document_is_empty(&output.document)
                && output.assets.is_empty()
                && utf8_source_is_blank(&source.bytes) =>
        {
            SourceContentEvidence::Empty
        }
        SourceContentEvidence::Unknown
            if document_is_asset_only(&output.document) && !output.assets.is_empty() =>
        {
            SourceContentEvidence::AssetsOnly
        }
        evidence => evidence,
    };
    let (code, message) = match evidence {
        SourceContentEvidence::Unknown => return Ok(output),
        SourceContentEvidence::Empty => {
            if !document_is_empty(&output.document) || !output.assets.is_empty() {
                return Err(ConversionError::Internal {
                    detail: "empty-source evidence conflicts with retained document content".into(),
                });
            }
            (EMPTY_SOURCE_REASON_CODE, "source was fully scanned and contains no visible content")
        }
        SourceContentEvidence::AssetsOnly => {
            if !document_is_asset_only(&output.document) || output.assets.is_empty() {
                return Err(ConversionError::Internal {
                    detail: "asset-only evidence requires asset-only IR and retained assets".into(),
                });
            }
            (ASSET_ONLY_REASON_CODE, "source contains only asset-backed structured content")
        }
    };

    // Hold a conservative peak before allocating the small audit diagnostic;
    // `account_retained` then transfers the exact retained charge to output.
    let allocation_guard = context.reserve_memory(EVIDENCE_DIAGNOSTIC_PEAK_BYTES)?;
    output.diagnostics.try_reserve(1).map_err(|error| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("cannot reserve source-content diagnostic: {error}"),
    })?;
    output.diagnostics.push(Diagnostic {
        code: code.into(),
        severity: DiagnosticSeverity::Info,
        message: message.into(),
        locator: None,
    });
    output = output.account_retained(context)?;
    drop(allocation_guard);
    Ok(output)
}

fn utf8_source_is_blank(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    std::str::from_utf8(bytes).is_ok_and(|text| text.chars().all(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Asset, AssetId, Block, BlockNode, Document, ExecutionOptions, NodeId, Provenance,
        ProvenanceKind, ResourceLimits, SourceLocator, SourceMetadata,
    };
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn source(bytes: &'static [u8]) -> ResolvedInput {
        ResolvedInput { metadata: SourceMetadata::default(), bytes: Arc::from(bytes) }
    }

    #[test]
    fn blank_markdown_gets_complete_audit_evidence() {
        let output = attach_evidence(
            ConverterOutput::default(),
            &source(b"\xef\xbb\xbf \r\n"),
            InputFormat::Markdown,
            &context(),
        )
        .unwrap();
        assert_eq!(output.diagnostics[0].code, EMPTY_SOURCE_REASON_CODE);
        assert_eq!(output.diagnostics[0].severity, DiagnosticSeverity::Info);
    }

    #[test]
    fn asset_only_ir_is_certified_without_format_special_cases() {
        let id = AssetId("asset".into());
        let output = ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("image".into()),
                    block: Block::Image { asset: id.clone(), alt: None },
                    provenance: Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "test".into(),
                        locator: SourceLocator::default(),
                        confidence: None,
                    },
                }],
                ..Document::default()
            },
            vec![Asset {
                id,
                filename: Some("asset.bin".into()),
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                external_uri: None,
            }],
            Vec::new(),
        );
        let output =
            attach_evidence(output, &source(b"asset"), InputFormat::Text, &context()).unwrap();
        assert_eq!(output.diagnostics[0].code, ASSET_ONLY_REASON_CODE);
    }
}

#[cfg(test)]
#[path = "result_policy/integration_tests.rs"]
mod integration_tests;
