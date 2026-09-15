//! Preserve source bytes when a completed analysis yields no usable body.
use into_markdown_core::{
    Asset, AssetId, AssetMode, Block, BlockNode, ConversionError, ConversionOptions,
    ConverterOutput, Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, Inline, NodeId,
    Provenance, ProvenanceKind, ResolvedInput, SourceLocator,
};
use sha2::{Digest, Sha256};

pub(crate) fn empty_body(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
    media_type: &str,
    reason: &str,
    mut output: ConverterOutput,
) -> Result<ConverterOutput, ConversionError> {
    if options.error_policy != ErrorPolicy::BestEffort
        || options.output.asset_mode == AssetMode::Omit
    {
        return Err(ConversionError::EmptyContent);
    }
    context.checkpoint()?;
    if input.bytes.len() as u64 > options.limits.max_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: "original source attachment exceeds the requested asset limit".into(),
        });
    }
    let reservation = context.reserve_memory((input.bytes.len() as u64).saturating_add(65536))?;
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
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "original source attachment allocation failed".into(),
    })?;
    bytes.extend_from_slice(&input.bytes);
    let provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "builtin.source-recovery".into(),
        locator: SourceLocator::default(),
        confidence: None,
    };
    output.document.blocks.push(BlockNode {
        id: NodeId("source-recovery-reason".into()),
        provenance: provenance.clone(),
        block: Block::Paragraph(vec![Inline::Text { value: reason.into(), marks: Vec::new() }]),
    });
    output.document.blocks.push(BlockNode {
        id: NodeId("source-recovery-attachment".into()),
        provenance,
        block: Block::Image {
            asset: AssetId(id.clone()),
            alt: Some("Original source — complete file".into()),
        },
    });
    output.assets.push(Asset {
        id: AssetId(id.clone()),
        filename: Some(format!("{id}.{extension}")),
        media_type: media_type.into(),
        bytes,
        external_uri: None,
    });
    output.diagnostics.push(Diagnostic {
        code: "conversion.recovery.originalFile".into(),
        severity: DiagnosticSeverity::Warning,
        message: reason.into(),
        locator: None,
    });
    output.attach_memory_reservation(context, reservation)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        ConversionOutcome, ExecutionOptions, SourceMetadata, conversion_outcome,
    };
    use std::sync::Arc;

    #[test]
    fn original_recovery_is_hash_bound_and_reports_degradation() {
        let input = ResolvedInput {
            bytes: Arc::from(&b"source payload"[..]),
            metadata: SourceMetadata {
                name: Some("silent.wav".into()),
                ..SourceMetadata::default()
            },
        };
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let output = empty_body(
            &input,
            &options,
            &context,
            "audio/wav",
            "No accepted speech text",
            ConverterOutput::default(),
        )
        .unwrap();
        assert_eq!(output.assets[0].bytes.as_slice(), input.bytes.as_ref());
        assert_eq!(output.assets[0].id.0, format!("source-{:x}", Sha256::digest(&input.bytes)));
        assert_eq!(conversion_outcome(&output.diagnostics), ConversionOutcome::Degraded);
        output.document.validate().unwrap();
        assert!(context.reserved_memory_bytes() > 0);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn strict_omitted_assets_and_explicit_budget_keep_their_real_results() {
        let input = ResolvedInput {
            bytes: Arc::from(&b"source payload"[..]),
            metadata: SourceMetadata::default(),
        };
        for kind in 0..3 {
            let mut options = ConversionOptions::default();
            match kind {
                0 => options.error_policy = ErrorPolicy::Strict,
                1 => options.output.asset_mode = AssetMode::Omit,
                _ => options.limits.max_memory_bytes = 1,
            }
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let error = empty_body(
                &input,
                &options,
                &context,
                "text/html",
                "No visible body",
                ConverterOutput::default(),
            )
            .unwrap_err();
            if kind == 2 {
                assert!(matches!(error, ConversionError::ResourceLimit { .. }));
            } else {
                assert!(matches!(error, ConversionError::EmptyContent));
            }
            assert_eq!(context.reserved_memory_bytes(), 0);
        }
    }
}
