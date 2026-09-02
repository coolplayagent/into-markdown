//! Ordered emission of rendered converter artifacts to a caller-owned sink.

use crate::rendering::RenderedArtifacts;
use into_markdown_core::{
    ArtifactSink, ArtifactSinkCapabilities, AssetId, AssetStreamInfo, Block, BlockNode,
    ConversionError, ConversionOptions, ConversionSummary, Diagnostic, DiagnosticSeverity,
    DocumentStreamEvent, ErrorPolicy, ExecutionContext, InputFormat, SourceLocator, TemporaryFile,
};
use sha2::{Digest as _, Sha256};
use std::io::{Read as _, Seek as _, SeekFrom};

const EMIT_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct AssetSpool {
    items: Vec<SpooledAsset>,
    content: into_markdown_core::ResultContent,
    payload_only: u64,
    external_only: u64,
    dual: u64,
}

struct SpooledAsset {
    info: AssetStreamInfo,
    file: Option<TemporaryFile>,
    omitted: bool,
}

#[allow(clippy::too_many_lines)] // Payload accounting, omission, and diagnostics form one transaction.
pub(crate) fn spool_assets(
    rendered: &mut RenderedArtifacts,
    options: &ConversionOptions,
    persist_payloads: bool,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if rendered.asset_spool.is_some() {
        return Err(ConversionError::Internal {
            detail: "asset payloads were spooled twice".into(),
        });
    }
    rendered.output.diagnostics.try_reserve(rendered.output.assets.len()).map_err(|error| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("cannot reserve asset-spool diagnostics: {error}"),
        }
    })?;
    let mut items = Vec::new();
    items.try_reserve_exact(rendered.output.assets.len()).map_err(|error| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("cannot reserve asset-spool index: {error}"),
        }
    })?;
    let mut payload_only = 0_u64;
    let mut external_only = 0_u64;
    let mut dual = 0_u64;
    let mut payload_spooling_disabled = false;
    for asset in &mut rendered.output.assets {
        context.checkpoint()?;
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset payload size is not representable".into(),
            })?;
        match (!asset.bytes.is_empty(), asset.external_uri.is_some()) {
            (true, false) => payload_only += 1,
            (false, true) => external_only += 1,
            (true, true) => dual += 1,
            (false, false) => {}
        }
        let digest = (!asset.bytes.is_empty()).then(|| Sha256::digest(&asset.bytes).into());
        let mut file = None;
        let mut omitted = false;
        if !asset.bytes.is_empty() && persist_payloads && !payload_spooling_disabled {
            match context.temporary_file("into-md-engine-asset").and_then(|mut temporary| {
                for chunk in asset.bytes.chunks(EMIT_CHUNK_BYTES) {
                    temporary.write_all_checked(chunk)?;
                }
                temporary.flush()?;
                Ok(temporary)
            }) {
                Ok(temporary) => file = Some(temporary),
                Err(
                    error @ ConversionError::ResourceLimit { limit: "max_temporary_bytes", .. },
                ) if options.error_policy == ErrorPolicy::BestEffort => {
                    omitted = true;
                    payload_spooling_disabled = true;
                    payload_only =
                        payload_only.saturating_sub(u64::from(asset.external_uri.is_none()));
                    dual = dual.saturating_sub(u64::from(asset.external_uri.is_some()));
                    if asset.external_uri.is_some() {
                        external_only = external_only.saturating_add(1);
                    }
                    rendered.output.diagnostics.push(Diagnostic {
                        code: "resource.max_temporary_bytes.unitOmitted".into(),
                        severity: DiagnosticSeverity::Warning,
                        message: format!(
                            "resource limit max_temporary_bytes: configured={}, observed={}, action=omitted 1 asset payload; Markdown reference retained ({error})",
                            options.limits.max_temporary_bytes, size
                        ),
                        locator: locate_asset(&rendered.output.document.blocks, &asset.id).or_else(
                            || {
                                Some(SourceLocator {
                                    part: Some(
                                        asset
                                            .filename
                                            .clone()
                                            .unwrap_or_else(|| asset.id.0.clone()),
                                    ),
                                    ..SourceLocator::default()
                                })
                            },
                        ),
                    });
                }
                Err(error) => return Err(error),
            }
        } else if !asset.bytes.is_empty() && persist_payloads {
            omitted = true;
            payload_only = payload_only.saturating_sub(u64::from(asset.external_uri.is_none()));
            dual = dual.saturating_sub(u64::from(asset.external_uri.is_some()));
            if asset.external_uri.is_some() {
                external_only = external_only.saturating_add(1);
            }
            rendered.output.diagnostics.push(Diagnostic {
                code: "resource.max_temporary_bytes.unitOmitted".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "resource limit max_temporary_bytes: configured={}, observed={size}, action=omitted 1 subsequent asset payload; Markdown reference retained",
                    options.limits.max_temporary_bytes
                ),
                locator: locate_asset(&rendered.output.document.blocks, &asset.id).or_else(|| {
                    Some(SourceLocator {
                        part: Some(
                            asset.filename.clone().unwrap_or_else(|| asset.id.0.clone()),
                        ),
                        ..SourceLocator::default()
                    })
                }),
            });
        }
        let omit_asset = omitted && asset.external_uri.is_none();
        items.push(SpooledAsset {
            info: AssetStreamInfo {
                id: asset.id.clone(),
                filename: asset.filename.clone(),
                media_type: asset.media_type.clone(),
                size: if omitted { 0 } else { size },
                external_uri: asset.external_uri.clone(),
                content_sha256: if omitted { None } else { digest },
            },
            file,
            omitted: omit_asset,
        });
        // The spool now owns the publishable payload (or the diagnostic owns
        // its omission). Release this asset before staging the next one so
        // long asset sequences do not keep a second long-lived RAM copy.
        asset.bytes = Vec::new();
    }
    let result_content = into_markdown_core::classify_result(
        &rendered.output.document,
        &rendered.markdown,
        &rendered.output.assets,
        &rendered.output.diagnostics,
    )?;
    let output = std::mem::take(&mut rendered.output).discard_asset_payloads(context)?;
    rendered.output = output;
    rendered.asset_spool =
        Some(AssetSpool { items, content: result_content, payload_only, external_only, dual });
    Ok(())
}

pub(crate) fn emit(
    rendered: RenderedArtifacts,
    format: InputFormat,
    capabilities: ArtifactSinkCapabilities,
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<ConversionSummary, ConversionError> {
    let RenderedArtifacts {
        output,
        markdown,
        provenance,
        markdown_memory,
        provenance_memory,
        asset_spool,
    } = rendered;
    let result_content = asset_spool.as_ref().map_or_else(
        || {
            into_markdown_core::classify_result(
                &output.document,
                &markdown,
                &output.assets,
                &output.diagnostics,
            )
        },
        |spool| Ok(spool.content),
    )?;
    if capabilities.semantic_events {
        emit_document(&output, &provenance, sink, context)?;
    }
    if capabilities.markdown {
        for chunk in markdown.as_bytes().chunks(EMIT_CHUNK_BYTES) {
            context.checkpoint()?;
            sink.write_markdown(chunk)?;
        }
    }
    if capabilities.assets {
        if let Some(spool) = &asset_spool {
            emit_spooled_assets(spool, sink, context)?;
        } else {
            emit_assets(&output.assets, sink, context)?;
        }
    }
    let markdown_bytes =
        u64::try_from(markdown.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_output_bytes",
            detail: "rendered Markdown byte count cannot be represented".into(),
        })?;
    drop(provenance_memory);
    drop(markdown_memory);
    drop(markdown);
    Ok(if let Some(spool) = asset_spool {
        output.into_conversion_summary_with_asset_counts(
            format,
            markdown_bytes,
            result_content,
            spool.payload_only,
            spool.external_only,
            spool.dual,
        )
    } else {
        output.into_conversion_summary(format, markdown_bytes, result_content)
    })
}

fn emit_spooled_assets(
    spool: &AssetSpool,
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut buffer = vec![0_u8; EMIT_CHUNK_BYTES].into_boxed_slice();
    for asset in &spool.items {
        context.checkpoint()?;
        if asset.omitted {
            continue;
        }
        sink.begin_asset(&asset.info)?;
        if let Some(temporary) = &asset.file {
            let mut file = temporary.as_file()?.try_clone()?;
            file.seek(SeekFrom::Start(0))?;
            loop {
                context.checkpoint()?;
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                sink.write_asset(&buffer[..read])?;
            }
        }
        context.checkpoint()?;
        sink.end_asset()?;
    }
    Ok(())
}

fn locate_asset(nodes: &[BlockNode], id: &AssetId) -> Option<SourceLocator> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } if asset == id => {
                return Some(node.provenance.locator.clone());
            }
            Block::List { items, .. } => {
                for item in items {
                    if let Some(locator) = locate_asset(&item.blocks, id) {
                        return Some(locator);
                    }
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        if let Some(locator) = locate_asset(&cell.blocks, id) {
                            return Some(locator);
                        }
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                if let Some(locator) = locate_asset(blocks, id) {
                    return Some(locator);
                }
            }
            _ => {}
        }
    }
    None
}

fn emit_document(
    output: &into_markdown_core::ConverterOutput,
    provenance: &[into_markdown_core::Provenance],
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    sink.write_document_event(&DocumentStreamEvent::Metadata(&output.document.metadata))?;
    for block in &output.document.blocks {
        context.checkpoint()?;
        sink.write_document_event(&DocumentStreamEvent::RootBlock(block))?;
    }
    context.checkpoint()?;
    sink.finish_document(&output.diagnostics, provenance)
}

fn emit_assets(
    assets: &[into_markdown_core::Asset],
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for asset in assets {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset byte length cannot be represented".into(),
            })?;
        context.checkpoint()?;
        sink.begin_asset(&AssetStreamInfo {
            id: asset.id.clone(),
            filename: asset.filename.clone(),
            media_type: asset.media_type.clone(),
            size,
            external_uri: asset.external_uri.clone(),
            content_sha256: (!asset.bytes.is_empty()).then(|| Sha256::digest(&asset.bytes).into()),
        })?;
        for chunk in asset.bytes.chunks(EMIT_CHUNK_BYTES) {
            context.checkpoint()?;
            sink.write_asset(chunk)?;
        }
        context.checkpoint()?;
        sink.end_asset()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Asset, BlockNode, ConverterOutput, Document, ExecutionOptions, NodeId, Provenance,
        ProvenanceKind, ResourceLimits,
    };

    struct BytesSink {
        bytes: Vec<u8>,
        digest: Option<[u8; 32]>,
    }

    impl ArtifactSink for BytesSink {
        fn write_markdown(&mut self, _: &[u8]) -> Result<(), ConversionError> {
            Ok(())
        }
        fn begin_asset(&mut self, info: &AssetStreamInfo) -> Result<(), ConversionError> {
            self.digest = info.content_sha256;
            Ok(())
        }
        fn write_asset(&mut self, chunk: &[u8]) -> Result<(), ConversionError> {
            self.bytes.extend_from_slice(chunk);
            Ok(())
        }
        fn end_asset(&mut self) -> Result<(), ConversionError> {
            Ok(())
        }
    }

    fn artifacts(context: &ExecutionContext, bytes: Vec<u8>) -> RenderedArtifacts {
        let id = AssetId("diagram".into());
        let output = ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("image".into()),
                    block: Block::Image { asset: id.clone(), alt: Some("diagram".into()) },
                    provenance: Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "fixture".into(),
                        locator: SourceLocator { page: Some(1), ..Default::default() },
                        confidence: Some(1.0),
                    },
                }],
                ..Default::default()
            },
            vec![Asset {
                id,
                filename: Some("diagram.png".into()),
                media_type: "image/png".into(),
                bytes,
                external_uri: None,
            }],
            vec![],
        )
        .account_retained(context)
        .unwrap();
        RenderedArtifacts {
            output,
            markdown: "![diagram](<asset.png>)".into(),
            provenance: vec![],
            markdown_memory: context.reserve_memory(0).unwrap(),
            provenance_memory: context.reserve_memory(0).unwrap(),
            asset_spool: None,
        }
    }

    #[test]
    fn spooled_assets_release_ram_and_stream_identical_bytes_and_hash() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: 32 * 1024 * 1024,
                max_temporary_bytes: 32 * 1024 * 1024,
                ..Default::default()
            },
        );
        let bytes = vec![0x5a; 8 * 1024 * 1024];
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let mut rendered = artifacts(&context, bytes.clone());
        let before = context.reserved_memory_bytes();
        spool_assets(&mut rendered, &ConversionOptions::default(), true, &context).unwrap();
        assert!(rendered.output.assets[0].bytes.is_empty());
        assert!(context.reserved_memory_bytes() < before);
        assert_eq!(context.reserved_temporary_bytes(), bytes.len() as u64);
        let mut sink = BytesSink { bytes: vec![], digest: None };
        emit_spooled_assets(rendered.asset_spool.as_ref().unwrap(), &mut sink, &context).unwrap();
        assert_eq!(sink.bytes, bytes);
        assert_eq!(sink.digest, Some(digest));
        drop(rendered);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }

    #[test]
    fn temporary_exhaustion_omits_payload_only_in_best_effort() {
        for (policy, succeeds) in [(ErrorPolicy::BestEffort, true), (ErrorPolicy::Strict, false)] {
            let context = ExecutionContext::new(
                ExecutionOptions::default(),
                ResourceLimits {
                    max_memory_bytes: 1024 * 1024,
                    max_temporary_bytes: 2,
                    ..Default::default()
                },
            );
            let mut rendered = artifacts(&context, vec![1, 2, 3, 4]);
            let mut options = ConversionOptions { error_policy: policy, ..Default::default() };
            options.limits.max_temporary_bytes = 2;
            let result = spool_assets(&mut rendered, &options, true, &context);
            assert_eq!(result.is_ok(), succeeds);
            if succeeds {
                assert!(rendered.output.diagnostics.iter().any(|item| {
                    item.code == "resource.max_temporary_bytes.unitOmitted"
                        && item.locator.as_ref().is_some_and(|locator| locator.page == Some(1))
                }));
                let mut sink = BytesSink { bytes: vec![], digest: None };
                emit_spooled_assets(rendered.asset_spool.as_ref().unwrap(), &mut sink, &context)
                    .unwrap();
                assert!(sink.bytes.is_empty());
            }
            drop(rendered);
            assert_eq!(context.reserved_temporary_bytes(), 0);
        }
    }
}
