//! CLI delivery policy for exceptional usable conversion results.

use crate::args::{AssetModeArg, EmitKind};
use crate::error::CliError;
use crate::output::BatchItemOutcome;
use into_markdown::{ConversionError, ConversionOutcome, ConversionSummary, ResultContent};
use std::path::PathBuf;

pub(crate) struct CommittedOutput {
    pub(crate) path: PathBuf,
    pub(crate) summary: ConversionSummary,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn validate_for_emit(
    summary: &ConversionSummary,
    emit: EmitKind,
    asset_mode: AssetModeArg,
) -> Result<(), CliError> {
    match summary.content().map_err(CliError::from)? {
        ResultContent::Markdown | ResultContent::EmptySource => Ok(()),
        ResultContent::AssetsOnly => {
            let (payloads, external_references) = match emit {
                EmitKind::Markdown => (false, false),
                EmitKind::IrJson => (asset_mode == AssetModeArg::Extract, false),
                EmitKind::ResultJson => (true, true),
                EmitKind::Bundle => (true, false),
            };
            if summary.assets_are_deliverable(payloads, external_references) {
                Ok(())
            } else {
                Err(CliError::from(ConversionError::EmptyContent)
                    .with_detected_format(summary.format))
            }
        }
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
        ASSET_ONLY_REASON_CODE, Asset, AssetId, Block, BlockNode, ConversionResult, Diagnostic,
        DiagnosticSeverity, Document, NodeId, Provenance, ProvenanceKind, SourceLocator,
    };

    fn asset_only(external: bool) -> ConversionResult {
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
                bytes: if external { Vec::new() } else { vec![1] },
                external_uri: external.then(|| "https://example.invalid/asset.bin".into()),
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
        let summary = asset_only(false).into_summary();
        assert!(validate_for_emit(&summary, EmitKind::ResultJson, AssetModeArg::Omit).is_ok());
        assert!(validate_for_emit(&summary, EmitKind::Bundle, AssetModeArg::Omit).is_ok());
        assert!(validate_for_emit(&summary, EmitKind::IrJson, AssetModeArg::Extract).is_ok());
        let error =
            validate_for_emit(&summary, EmitKind::Markdown, AssetModeArg::Extract).unwrap_err();
        assert_eq!(error.reason_code(), "emptyContent");
    }

    #[test]
    fn external_only_assets_require_external_metadata_in_the_selected_wire_format() {
        let summary = asset_only(true).into_summary();
        assert!(validate_for_emit(&summary, EmitKind::ResultJson, AssetModeArg::Omit).is_ok());
        for (emit, mode) in [
            (EmitKind::Markdown, AssetModeArg::Omit),
            (EmitKind::IrJson, AssetModeArg::Extract),
            (EmitKind::Bundle, AssetModeArg::Omit),
        ] {
            assert_eq!(
                validate_for_emit(&summary, emit, mode).unwrap_err().reason_code(),
                "emptyContent"
            );
        }
    }
}
