//! CLI delivery policy for exceptional usable conversion results.

use crate::args::{AssetModeArg, EmitKind};
use crate::error::CliError;
use crate::output::BatchItemOutcome;
use into_markdown::{
    ConversionError, ConversionOutcome, ConversionResult, Diagnostic, InputFormat, ResultContent,
};
use std::path::PathBuf;

pub(crate) struct CommittedOutput {
    pub(crate) path: PathBuf,
    pub(crate) format: Option<InputFormat>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) warnings: Vec<String>,
    pub(crate) outcome: ConversionOutcome,
    pub(crate) reason_code: Option<String>,
}

pub(crate) fn validate_for_emit(
    result: &ConversionResult,
    emit: EmitKind,
    asset_mode: AssetModeArg,
) -> Result<(), CliError> {
    match result.content().map_err(CliError::from)? {
        ResultContent::Markdown | ResultContent::EmptySource => Ok(()),
        ResultContent::AssetsOnly
            if matches!(emit, EmitKind::Bundle | EmitKind::ResultJson)
                || emit == EmitKind::IrJson && asset_mode == AssetModeArg::Extract =>
        {
            Ok(())
        }
        ResultContent::AssetsOnly => Err(CliError::from(ConversionError::EmptyContent)
            .with_detected_format(result.detected_format())),
    }
}

pub(crate) const fn batch_outcome(outcome: ConversionOutcome) -> BatchItemOutcome {
    match outcome {
        ConversionOutcome::Complete => BatchItemOutcome::Complete,
        ConversionOutcome::Degraded => BatchItemOutcome::Degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::{
        ASSET_ONLY_REASON_CODE, Asset, AssetId, Block, BlockNode, Diagnostic, DiagnosticSeverity,
        Document, NodeId, Provenance, ProvenanceKind, SourceLocator,
    };

    fn asset_only() -> ConversionResult {
        let id = AssetId("asset".into());
        ConversionResult::new(
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
            String::new(),
            vec![Asset {
                id,
                filename: Some("asset.bin".into()),
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                external_uri: None,
            }],
            vec![Diagnostic {
                code: ASSET_ONLY_REASON_CODE.into(),
                severity: DiagnosticSeverity::Info,
                message: "asset only".into(),
                locator: None,
            }],
            Vec::new(),
        )
    }

    #[test]
    fn asset_only_delivery_requires_a_structured_or_extracted_target() {
        let result = asset_only();
        assert!(validate_for_emit(&result, EmitKind::ResultJson, AssetModeArg::Omit).is_ok());
        assert!(validate_for_emit(&result, EmitKind::Bundle, AssetModeArg::Omit).is_ok());
        assert!(validate_for_emit(&result, EmitKind::IrJson, AssetModeArg::Extract).is_ok());
        let error =
            validate_for_emit(&result, EmitKind::Markdown, AssetModeArg::Extract).unwrap_err();
        assert_eq!(error.reason_code(), "emptyContent");
    }
}
