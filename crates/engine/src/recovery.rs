//! Versioned, crash-safe conversion task checkpoints.

mod store;

pub use store::{RecoveryStore, RecoveryToken, TaskCheckpoint, TaskPhase};

use super::{
    Attempt, Engine, collect_provenance_preflighted, invoke_converter_preflighted,
    invoke_enrichers, invoke_renderer_preflighted, measured_input_bytes, normalize_confidence,
    provenance_inventory_bytes,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionRequest, ConversionResult,
    ConverterOutput, Diagnostic, Document, ErrorCode, ExecutionContext, ExecutionStage,
    MediaCheckpoint, MediaCheckpointBackend, ProbeOutcome, Provenance, RecoveredMediaCheckpoint,
    ResourceReservation, SourceLocator, SourceMetadata, canonical_external_asset_uri,
    estimate_retained_result, estimate_validation_working_set,
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
enum CheckpointPayloadWire {
    Media {
        checkpoint: MediaCheckpoint,
    },
    Converted {
        document: Document,
        assets: Vec<CheckpointAssetWire>,
        diagnostics: Vec<Diagnostic>,
    },
    Succeeded {
        document: Document,
        markdown: String,
        assets: Vec<CheckpointAssetWire>,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<Provenance>,
    },
}

enum CheckpointPayload {
    Media(MediaCheckpoint),
    Converted {
        document: Document,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
    },
    Succeeded {
        document: Document,
        markdown: String,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<Provenance>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointAssetWire {
    id: AssetId,
    filename: Option<String>,
    media_type: String,
    decoded_bytes: u64,
    data_base64: String,
    external_uri: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
enum CheckpointPayloadRef<'a> {
    Media {
        checkpoint: &'a MediaCheckpoint,
    },
    Converted {
        document: &'a Document,
        assets: CheckpointAssetsRef<'a>,
        diagnostics: &'a [Diagnostic],
    },
    Succeeded {
        document: &'a Document,
        markdown: &'a str,
        assets: CheckpointAssetsRef<'a>,
        diagnostics: &'a [Diagnostic],
        provenance: &'a [Provenance],
    },
}

struct CheckpointAssetsRef<'a>(&'a [Asset]);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointAssetRef<'a> {
    id: &'a AssetId,
    filename: Option<&'a str>,
    media_type: &'a str,
    decoded_bytes: u64,
    data_base64: CanonicalBase64<'a>,
    external_uri: Option<&'a str>,
}

struct CanonicalBase64<'a>(&'a [u8]);

impl fmt::Display for CanonicalBase64<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const RAW_CHUNK_BYTES: usize = 3 * 1024;
        let mut encoded = [0_u8; 4 * 1024];
        for chunk in self.0.chunks(RAW_CHUNK_BYTES) {
            let written = STANDARD.encode_slice(chunk, &mut encoded).map_err(|_| fmt::Error)?;
            let text = std::str::from_utf8(&encoded[..written]).map_err(|_| fmt::Error)?;
            formatter.write_str(text)?;
        }
        Ok(())
    }
}

impl Serialize for CanonicalBase64<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl Serialize for CheckpointAssetsRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for asset in self.0 {
            sequence.serialize_element(&CheckpointAssetRef {
                id: &asset.id,
                filename: asset.filename.as_deref(),
                media_type: &asset.media_type,
                decoded_bytes: u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX),
                data_base64: CanonicalBase64(&asset.bytes),
                external_uri: asset.external_uri.as_deref(),
            })?;
        }
        sequence.end()
    }
}

impl CheckpointPayloadWire {
    fn decode(
        self,
        context: &ExecutionContext,
        memory: &mut ResourceReservation,
        request: &ConversionRequest,
    ) -> Result<CheckpointPayload, ConversionError> {
        Ok(match self {
            Self::Media { checkpoint } => CheckpointPayload::Media(checkpoint),
            Self::Converted { document, assets, diagnostics } => CheckpointPayload::Converted {
                document,
                assets: decode_checkpoint_assets(assets, context, memory, request)?,
                diagnostics,
            },
            Self::Succeeded { document, markdown, assets, diagnostics, provenance } => {
                CheckpointPayload::Succeeded {
                    document,
                    markdown,
                    assets: decode_checkpoint_assets(assets, context, memory, request)?,
                    diagnostics,
                    provenance,
                }
            }
        })
    }
}

struct RecoveryMediaCheckpointBackend {
    store: RecoveryStore,
    token: RecoveryToken,
    input_fingerprint: String,
    options_fingerprint: String,
}

impl MediaCheckpointBackend for RecoveryMediaCheckpointBackend {
    fn load(
        &self,
        context: &ExecutionContext,
    ) -> Result<Option<RecoveredMediaCheckpoint>, ConversionError> {
        let Some(store::LoadedCheckpoint { metadata, payload, memory }) =
            self.store.load::<CheckpointPayloadWire>(&self.token, context)?
        else {
            return Ok(None);
        };
        if metadata.input_fingerprint != self.input_fingerprint
            || metadata.options_fingerprint != self.options_fingerprint
        {
            return Err(recovery_error("incompatible", "media checkpoint fingerprints changed"));
        }
        let CheckpointPayloadWire::Media { checkpoint } = payload else {
            return Ok(None);
        };
        if metadata.phase != TaskPhase::Media {
            return Err(recovery_error(
                "corrupt",
                "media checkpoint payload does not match its phase",
            ));
        }
        checkpoint.validate()?;
        Ok(Some(RecoveredMediaCheckpoint::new(checkpoint, memory)))
    }

    fn commit(
        &self,
        checkpoint: &MediaCheckpoint,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        checkpoint.validate()?;
        self.store.replace_media(
            &self.token,
            context,
            &self.input_fingerprint,
            &self.options_fingerprint,
            &CheckpointPayloadRef::Media { checkpoint },
        )
    }
}

fn decode_checkpoint_assets(
    wire_assets: Vec<CheckpointAssetWire>,
    context: &ExecutionContext,
    memory: &mut ResourceReservation,
    request: &ConversionRequest,
) -> Result<Vec<Asset>, ConversionError> {
    if wire_assets.len() > into_markdown_core::MAX_DTO_ASSETS {
        return Err(ConversionError::ResourceLimit {
            limit: "max_assets",
            detail: format!("{} > {}", wire_assets.len(), into_markdown_core::MAX_DTO_ASSETS),
        });
    }
    let mut total = 0_u64;
    let mut ids = std::collections::BTreeSet::new();
    for wire in &wire_assets {
        context.checkpoint()?;
        total = preflight_checkpoint_asset(wire, &mut ids, request, total)?;
    }
    let vector_bytes = u64::try_from(wire_assets.len())
        .ok()
        .and_then(|count| {
            count.checked_mul(u64::try_from(std::mem::size_of::<Asset>()).unwrap_or(u64::MAX))
        })
        .ok_or_else(|| recovery_error("limit", "checkpoint asset vector size overflowed"))?;
    memory.grow(total)?;
    memory.grow(vector_bytes)?;
    let mut assets = Vec::new();
    assets.try_reserve_exact(wire_assets.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "checkpoint asset vector allocation failed".into(),
    })?;
    for wire in wire_assets {
        context.checkpoint()?;
        let capacity = usize::try_from(wire.decoded_bytes).map_err(|_| {
            recovery_error("limit", "checkpoint decoded asset length is not representable")
        })?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("checkpoint asset {} allocation failed", wire.id.0),
        })?;
        bytes.resize(capacity, 0);
        let mut written = 0_usize;
        for chunk in wire.data_base64.as_bytes().chunks(64 * 1024) {
            context.checkpoint()?;
            let decoded = STANDARD.decode_slice(chunk, &mut bytes[written..]).map_err(|_| {
                recovery_error("corrupt", "checkpoint asset base64 is not canonical")
            })?;
            written = written.checked_add(decoded).ok_or_else(|| {
                recovery_error("limit", "checkpoint decoded asset length overflowed")
            })?;
        }
        if written != capacity {
            return Err(recovery_error(
                "corrupt",
                "checkpoint asset decoded length does not match its declaration",
            ));
        }
        assets.push(Asset {
            id: wire.id,
            filename: wire.filename,
            media_type: wire.media_type,
            bytes,
            external_uri: wire.external_uri,
        });
    }
    Ok(assets)
}

fn preflight_checkpoint_asset<'a>(
    wire: &'a CheckpointAssetWire,
    ids: &mut std::collections::BTreeSet<&'a str>,
    request: &ConversionRequest,
    total: u64,
) -> Result<u64, ConversionError> {
    let decoded = canonical_base64_decoded_len(wire)?;
    let id = wire.id.0.as_str();
    if id.trim().is_empty() || id.chars().any(char::is_control) || !ids.insert(id) {
        return Err(recovery_error(
            "corrupt",
            "checkpoint asset IDs must be non-empty, unique, and control-free",
        ));
    }
    if !safe_media_type(&wire.media_type) {
        return Err(recovery_error("corrupt", "checkpoint asset MIME type is unsafe"));
    }
    match wire.external_uri.as_deref() {
        Some(uri) if canonical_external_asset_uri(uri).as_deref() != Some(uri) => {
            return Err(recovery_error(
                "corrupt",
                "checkpoint asset external URI is not canonical HTTP(S)",
            ));
        }
        None if decoded == 0 => {
            return Err(recovery_error(
                "corrupt",
                "checkpoint asset has neither bytes nor an external URI",
            ));
        }
        _ => {}
    }
    if decoded > request.options.limits.max_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: format!(
                "asset {}: {decoded} > {}",
                wire.id.0, request.options.limits.max_asset_bytes
            ),
        });
    }
    let total = total.checked_add(decoded).ok_or_else(|| ConversionError::ResourceLimit {
        limit: "max_total_asset_bytes",
        detail: "checkpoint asset byte count overflowed".into(),
    })?;
    if total > request.options.limits.max_total_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: format!("{total} > {}", request.options.limits.max_total_asset_bytes),
        });
    }
    Ok(total)
}

fn canonical_base64_decoded_len(wire: &CheckpointAssetWire) -> Result<u64, ConversionError> {
    let encoded = wire.data_base64.as_bytes();
    if !encoded.len().is_multiple_of(4) {
        return Err(recovery_error(
            "corrupt",
            "checkpoint asset base64 must use canonical padding",
        ));
    }
    let padding = match encoded {
        [.., b'=', b'='] => 2_u64,
        [.., b'='] => 1_u64,
        _ => 0_u64,
    };
    let content_end = encoded.len().saturating_sub(usize::try_from(padding).unwrap_or(0));
    if encoded[..content_end]
        .iter()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')))
        || encoded[content_end..].iter().any(|byte| *byte != b'=')
    {
        return Err(recovery_error(
            "corrupt",
            "checkpoint asset base64 contains a non-canonical symbol",
        ));
    }
    let decoded = u64::try_from(encoded.len() / 4)
        .ok()
        .and_then(|groups| groups.checked_mul(3))
        .and_then(|bytes| bytes.checked_sub(padding))
        .ok_or_else(|| recovery_error("limit", "checkpoint decoded asset length overflowed"))?;
    if decoded != wire.decoded_bytes {
        return Err(recovery_error(
            "corrupt",
            "checkpoint asset decoded length does not match its declaration",
        ));
    }
    Ok(decoded)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn convert(
    engine: &Engine,
    mut request: ConversionRequest,
    store: &RecoveryStore,
    token: &RecoveryToken,
) -> Result<ConversionResult, ConversionError> {
    if request.options.text.charset.is_none() {
        request.options.text.charset.clone_from(&request.hint.charset);
    }
    let options_fingerprint = fingerprint_json(&(request.hint.clone(), request.options.clone()))?;
    let execution = std::mem::take(&mut request.execution);
    let context = ExecutionContext::new(execution, request.options.limits.clone());
    context.report(ExecutionStage::Resolving, None, None, None::<String>)?;
    let mut source = engine.resolve_input(&request.input, &request.options, &context).await?;
    measured_input_bytes(source.input(), &request.options)?;
    source.ensure_memory_reservation(&context)?;
    let input_fingerprint = fingerprint_input(&source.input().bytes, &source.input().metadata)?;

    // The cross-process token lock linearizes conversion and publication. A
    // later invocation can only observe and return the persisted winner.
    let _task_lock = store.lock(token, &context)?;
    if let Some(metadata) = store.inspect(token)? {
        if metadata.input_fingerprint != input_fingerprint {
            return Err(recovery_error("incompatible", "resolved input fingerprint changed"));
        }
        if metadata.options_fingerprint != options_fingerprint {
            return Err(recovery_error("incompatible", "conversion configuration changed"));
        }
    }
    context.install_media_checkpoint_backend(Arc::new(RecoveryMediaCheckpointBackend {
        store: store.clone(),
        token: token.clone(),
        input_fingerprint: input_fingerprint.clone(),
        options_fingerprint: options_fingerprint.clone(),
    }))?;
    let existing = match store.load::<CheckpointPayloadWire>(token, &context)? {
        Some(store::LoadedCheckpoint { metadata, payload, mut memory }) => {
            let payload = payload.decode(&context, &mut memory, &request)?;
            Some(store::LoadedCheckpoint { metadata, payload, memory })
        }
        None => None,
    };
    if let Some(checkpoint) = &existing {
        if checkpoint.metadata.input_fingerprint != input_fingerprint {
            return Err(recovery_error("incompatible", "resolved input fingerprint changed"));
        }
        if checkpoint.metadata.options_fingerprint != options_fingerprint {
            return Err(recovery_error("incompatible", "conversion configuration changed"));
        }
    }

    let existing = match existing {
        Some(store::LoadedCheckpoint {
            payload:
                CheckpointPayload::Succeeded { document, markdown, assets, diagnostics, provenance },
            metadata,
            memory,
        }) => {
            if metadata.phase != TaskPhase::Succeeded {
                return Err(recovery_error(
                    "corrupt",
                    "checkpoint payload does not match its phase",
                ));
            }
            let validation_bytes =
                estimate_validation_working_set(&document, &assets, &diagnostics).map_err(
                    |error| {
                        recovery_error(
                            "corrupt",
                            format!("checkpoint contains invalid document IR: {error}"),
                        )
                    },
                )?;
            let validation_memory = context.reserve_memory(validation_bytes)?;
            document.validate().map_err(|error| {
                recovery_error(
                    "corrupt",
                    format!("checkpoint contains invalid document IR: {error}"),
                )
            })?;
            validate_asset_inventory(&document, &assets, &request)?;
            validate_diagnostics(&diagnostics)?;
            if !provenance_matches(&document.blocks, &provenance)
                || provenance.len() > into_markdown_core::MAX_DTO_PROVENANCE
            {
                return Err(recovery_error(
                    "corrupt",
                    "checkpoint provenance does not match document reading order",
                ));
            }
            drop(validation_memory);
            let result = ConversionResult::from_recovered_accounted_parts(
                document,
                markdown,
                assets,
                diagnostics,
                provenance,
                &context,
                memory,
            )?;
            let result = validate_recovered_success(engine, &request, &context, result).await?;
            context.report(ExecutionStage::Completed, Some(1), Some(1), Some("resumed"))?;
            return Ok(result);
        }
        existing => existing,
    };

    let existing = match existing {
        Some(store::LoadedCheckpoint {
            payload: CheckpointPayload::Media(checkpoint),
            metadata,
            memory,
        }) => {
            if metadata.phase != TaskPhase::Media {
                return Err(recovery_error(
                    "corrupt",
                    "media checkpoint payload does not match its phase",
                ));
            }
            checkpoint.validate()?;
            drop(memory);
            None
        }
        existing => existing,
    };

    let output = if let Some(store::LoadedCheckpoint {
        payload: CheckpointPayload::Converted { document, assets, diagnostics },
        metadata,
        memory,
    }) = existing
    {
        if metadata.phase != TaskPhase::Converted {
            return Err(recovery_error("corrupt", "checkpoint payload does not match its phase"));
        }
        let validation_bytes = estimate_validation_working_set(&document, &assets, &diagnostics)
            .map_err(|error| {
                recovery_error(
                    "corrupt",
                    format!("checkpoint contains invalid document IR: {error}"),
                )
            })?;
        let validation_memory = context.reserve_memory(validation_bytes)?;
        document.validate().map_err(|error| {
            recovery_error("corrupt", format!("checkpoint contains invalid document IR: {error}"))
        })?;
        validate_asset_inventory(&document, &assets, &request)?;
        validate_diagnostics(&diagnostics)?;
        drop(validation_memory);
        context.report(ExecutionStage::Converting, Some(1), Some(1), Some("resumed"))?;
        ConverterOutput::new_with_memory_reservation(
            document,
            assets,
            diagnostics,
            &context,
            memory,
        )?
    } else {
        context.report(ExecutionStage::Detecting, None, None, None::<String>)?;
        let candidates = engine.detect_formats(source.input(), &request.hint, &context).await?;
        if candidates.is_empty() {
            return Err(ConversionError::Unsupported {
                detail: "format detectors produced no candidates".into(),
            });
        }
        context.report(ExecutionStage::Probing, Some(0), None, None::<String>)?;
        let mut attempts = Vec::new();
        for candidate in &candidates {
            for converter in &engine.converters {
                if converter.supported_formats().contains(&candidate.format)
                    && let ProbeOutcome::Match { confidence } =
                        context.run(converter.probe(source.input(), candidate, &context)).await??
                {
                    attempts.push(Attempt {
                        converter: Arc::clone(converter),
                        candidate: candidate.clone(),
                        explicit: candidate.explicit,
                        confidence: candidate.confidence * normalize_confidence(confidence),
                        priority: converter.priority(),
                    });
                }
            }
        }
        attempts.sort_by(|left, right| {
            right
                .explicit
                .cmp(&left.explicit)
                .then_with(|| right.confidence.total_cmp(&left.confidence))
                .then_with(|| right.priority.cmp(&left.priority))
                .then_with(|| left.converter.id().cmp(right.converter.id()))
        });
        let attempt = attempts.into_iter().next().ok_or_else(|| ConversionError::NoConverter {
            format: candidates
                .iter()
                .map(|value| value.format.as_str())
                .collect::<Vec<_>>()
                .join(","),
        })?;
        context.report(ExecutionStage::Converting, None, None, Some(attempt.converter.id()))?;
        let output = invoke_converter_preflighted(
            attempt.converter.as_ref(),
            source.input(),
            &attempt.candidate,
            &request.options,
            &engine.services,
            &context,
            |output| {
                validate_asset_inventory(&output.document, &output.assets, &request)?;
                validate_diagnostics(&output.diagnostics)
            },
        )
        .await?;
        let output = invoke_enrichers(
            &engine.enrichers,
            output,
            attempt.converter.id(),
            attempt.candidate.format,
            &request.options,
            &engine.services,
            &context,
        )
        .await?;
        let output = crate::result_policy::attach_evidence(
            output,
            source.input(),
            attempt.candidate.format,
            &context,
        )?;
        validate_asset_inventory(&output.document, &output.assets, &request)?;
        validate_diagnostics(&output.diagnostics)?;
        store.commit(
            token,
            &context,
            &input_fingerprint,
            &options_fingerprint,
            TaskPhase::Converted,
            &CheckpointPayloadRef::Converted {
                document: &output.document,
                assets: CheckpointAssetsRef(&output.assets),
                diagnostics: &output.diagnostics,
            },
        )?;
        store.remove_media(token)?;
        output
    };

    let renderer = engine.renderer.as_ref().ok_or_else(|| ConversionError::Internal {
        detail: "no Markdown renderer is registered".into(),
    })?;
    context.report(ExecutionStage::Rendering, None, None, Some(renderer.id()))?;
    let (markdown, markdown_memory) = invoke_renderer_preflighted(
        renderer.as_ref(),
        &output.document,
        &output.assets,
        &request.options,
        &context,
    )
    .await?;
    let markdown_bytes =
        u64::try_from(markdown.capacity()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "rendered Markdown capacity cannot be represented as u64".into(),
        })?;
    let (provenance, provenance_memory) =
        collect_provenance_preflighted(&output.document.blocks, &context)?;
    let final_required = estimate_retained_result(
        &output.document,
        &markdown,
        &output.assets,
        &output.diagnostics,
        &provenance,
    )?;
    let final_memory = context.reserve_memory(
        final_required.saturating_sub(
            output
                .leased_memory_for(&context)
                .saturating_add(markdown_bytes)
                .saturating_add(provenance_inventory_bytes(&provenance)?),
        ),
    )?;
    let result = output.into_conversion_result(
        markdown,
        provenance,
        [Some(markdown_memory), Some(provenance_memory), Some(final_memory)],
    )?;
    result.content()?;
    store.commit(
        token,
        &context,
        &input_fingerprint,
        &options_fingerprint,
        TaskPhase::Succeeded,
        &CheckpointPayloadRef::Succeeded {
            document: &result.document,
            markdown: &result.markdown,
            assets: CheckpointAssetsRef(&result.assets),
            diagnostics: &result.diagnostics,
            provenance: &result.provenance,
        },
    )?;
    context.report(ExecutionStage::Completed, Some(1), Some(1), None::<String>)?;
    drop(source);
    Ok(result)
}

fn measured_asset_bytes(
    assets: &[Asset],
    request: &ConversionRequest,
) -> Result<u64, ConversionError> {
    let total = assets.iter().try_fold(0_u64, |total, asset| {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset size cannot be represented as u64".into(),
            })?;
        if size > request.options.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!(
                    "asset {}: {size} > {}",
                    asset.id.0, request.options.limits.max_asset_bytes
                ),
            });
        }
        total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: "asset byte count overflowed".into(),
        })
    })?;
    if total > request.options.limits.max_total_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: format!("{total} > {}", request.options.limits.max_total_asset_bytes),
        });
    }
    Ok(total)
}

fn validate_asset_inventory(
    document: &Document,
    assets: &[Asset],
    request: &ConversionRequest,
) -> Result<(), ConversionError> {
    if assets.len() > into_markdown_core::MAX_DTO_ASSETS {
        return Err(ConversionError::ResourceLimit {
            limit: "max_assets",
            detail: format!("{} > {}", assets.len(), into_markdown_core::MAX_DTO_ASSETS),
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for asset in assets {
        let id = asset.id.0.as_str();
        if id.trim().is_empty() || id.chars().any(char::is_control) || !ids.insert(id) {
            return Err(recovery_error(
                "corrupt",
                "checkpoint asset IDs must be non-empty, unique, and control-free",
            ));
        }
        if !safe_media_type(&asset.media_type) {
            return Err(recovery_error("corrupt", "checkpoint asset MIME type is unsafe"));
        }
        match asset.external_uri.as_deref() {
            Some(uri) if canonical_external_asset_uri(uri).as_deref() != Some(uri) => {
                return Err(recovery_error(
                    "corrupt",
                    "checkpoint asset external URI is not canonical HTTP(S)",
                ));
            }
            None if asset.bytes.is_empty() => {
                return Err(recovery_error(
                    "corrupt",
                    "checkpoint asset has neither bytes nor an external URI",
                ));
            }
            _ => {}
        }
    }
    validate_asset_references(&document.blocks, &ids)?;
    measured_asset_bytes(assets, request).map(|_| ())
}

fn validate_asset_references(
    nodes: &[BlockNode],
    inventory: &std::collections::BTreeSet<&str>,
) -> Result<(), ConversionError> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } if !inventory.contains(asset.0.as_str()) => {
                return Err(recovery_error(
                    "corrupt",
                    format!("checkpoint document references missing asset {}", asset.0),
                ));
            }
            Block::List { items, .. } => {
                for item in items {
                    validate_asset_references(&item.blocks, inventory)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        validate_asset_references(&cell.blocks, inventory)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => validate_asset_references(blocks, inventory)?,
            _ => {}
        }
    }
    Ok(())
}

fn validate_diagnostics(diagnostics: &[Diagnostic]) -> Result<(), ConversionError> {
    if diagnostics.len() > into_markdown_core::MAX_DTO_DIAGNOSTICS {
        return Err(recovery_error("limit", "checkpoint contains too many diagnostics"));
    }
    for diagnostic in diagnostics {
        if diagnostic.code.is_empty() || diagnostic.code.chars().any(char::is_control) {
            return Err(recovery_error(
                "corrupt",
                "checkpoint diagnostic code must be non-empty and control-free",
            ));
        }
        if let Some(locator) = &diagnostic.locator {
            validate_locator(locator)?;
        }
    }
    Ok(())
}

fn validate_locator(locator: &SourceLocator) -> Result<(), ConversionError> {
    if locator.byte_start.is_some() != locator.byte_end.is_some()
        || locator.byte_start.zip(locator.byte_end).is_some_and(|(start, end)| start > end)
        || locator.page == Some(0)
        || locator.slide == Some(0)
        || locator.sheet.as_ref().is_some_and(|value| value.trim().is_empty())
        || (locator.cell.is_some() && locator.sheet.is_none())
        || locator.bounds.is_some_and(|bounds| {
            !bounds.x.is_finite()
                || !bounds.y.is_finite()
                || !bounds.width.is_finite()
                || !bounds.height.is_finite()
                || bounds.width < 0.0
                || bounds.height < 0.0
        })
        || locator.time.is_some_and(|range| range.start_ms >= range.end_ms)
        || locator.part.as_deref().is_some_and(|part| {
            let drive = part.as_bytes().get(1) == Some(&b':')
                && part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
            part.is_empty()
                || part.starts_with('/')
                || drive
                || part.contains('\\')
                || part.chars().any(char::is_control)
                || part
                    .split('/')
                    .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        })
    {
        return Err(recovery_error("corrupt", "checkpoint diagnostic locator is invalid"));
    }
    Ok(())
}

fn safe_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else { return false };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && kind.bytes().chain(subtype.bytes()).all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
        })
}

async fn validate_recovered_success(
    engine: &Engine,
    request: &ConversionRequest,
    context: &ExecutionContext,
    result: ConversionResult,
) -> Result<ConversionResult, ConversionError> {
    let renderer = engine.renderer.as_ref().ok_or_else(|| ConversionError::Internal {
        detail: "no Markdown renderer is registered".into(),
    })?;
    context.report(ExecutionStage::Rendering, None, None, Some("validating checkpoint"))?;
    let regenerated = invoke_renderer_preflighted(
        renderer.as_ref(),
        &result.document,
        &result.assets,
        &request.options,
        context,
    )
    .await;
    let (regenerated_markdown, _regenerated_memory) = match regenerated {
        Ok(value) => value,
        Err(error)
            if matches!(
                error.code(),
                ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::ResourceLimit
            ) =>
        {
            return Err(error);
        }
        Err(error) => {
            return Err(recovery_error(
                "corrupt",
                format!("checkpoint result fails renderer validation: {error}"),
            ));
        }
    };
    if regenerated_markdown != result.markdown {
        return Err(recovery_error(
            "corrupt",
            "checkpoint Markdown does not match its document and assets",
        ));
    }
    result.content().map_err(|error| {
        recovery_error("corrupt", format!("checkpoint result fails content validation: {error}"))
    })?;
    Ok(result)
}

fn provenance_matches(nodes: &[BlockNode], inventory: &[Provenance]) -> bool {
    fn visit(nodes: &[BlockNode], inventory: &[Provenance], index: &mut usize) -> bool {
        for node in nodes {
            if inventory.get(*index) != Some(&node.provenance) {
                return false;
            }
            *index = index.saturating_add(1);
            let valid = match &node.block {
                Block::List { items, .. } => {
                    items.iter().all(|item| visit(&item.blocks, inventory, index))
                }
                Block::Table { rows, .. } => rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .all(|cell| visit(&cell.blocks, inventory, index)),
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => visit(blocks, inventory, index),
                _ => true,
            };
            if !valid {
                return false;
            }
        }
        true
    }
    let mut index = 0_usize;
    visit(nodes, inventory, &mut index) && index == inventory.len()
}

pub(crate) fn fingerprint_input(
    bytes: &[u8],
    metadata: &SourceMetadata,
) -> Result<String, ConversionError> {
    let metadata = serde_json::to_vec(&(
        metadata.name.as_deref(),
        metadata.media_type.as_deref(),
        metadata.uri.as_deref(),
        metadata.size,
    ))
    .map_err(|error| recovery_error("internal", format!("encode input metadata: {error}")))?;
    Ok(hash_parts(&[b"into-markdown-input-v1", bytes, &metadata]))
}

pub(crate) fn fingerprint_json<T: Serialize>(value: &T) -> Result<String, ConversionError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        recovery_error("internal", format!("encode recovery configuration: {error}"))
    })?;
    Ok(hash_parts(&[b"into-markdown-options-v1", &bytes]))
}

fn hash_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        hash.update(part);
    }
    encode_hex(&hash.finalize())
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut output, byte| {
        let _ = write!(&mut output, "{byte:02x}");
        output
    })
}

fn recovery_error(reason: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Recovery { reason, detail: detail.into() }
}

#[allow(clippy::needless_pass_by_value)]
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::EngineBuilder;
    use into_markdown_core::{
        BoxFuture, ConversionOptions, Converter, ExecutionOptions, FormatCandidate, FormatDetector,
        FormatHint, Inline, InputFormat, InputRef, MEDIA_CHECKPOINT_SCHEMA_VERSION,
        MarkdownRenderer, MediaCheckpointStage, NormalizedAudioIdentity, OutputEnricher,
        ResolvedInput, ResourceLimits, Services, SourceResolver, TimeRange,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{fs, fs::File, io::Write as _, path::Path};

    const TEST_INPUT_FINGERPRINT: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";
    const TEST_OPTIONS_FINGERPRINT: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    struct BytesResolver;

    impl SourceResolver for BytesResolver {
        fn id(&self) -> &'static str {
            "recovery.bytes"
        }

        fn supports(&self, input: &InputRef) -> bool {
            matches!(input, InputRef::Bytes { .. })
        }

        fn resolve<'a>(
            &'a self,
            input: &'a InputRef,
            _: &'a ConversionOptions,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
            Box::pin(async move {
                let InputRef::Bytes { data, name } = input else {
                    return Err(ConversionError::Unsupported { detail: "expected bytes".into() });
                };
                Ok(ResolvedInput {
                    bytes: Arc::clone(data),
                    metadata: SourceMetadata {
                        name: name.clone(),
                        size: u64::try_from(data.len()).unwrap_or(u64::MAX),
                        ..SourceMetadata::default()
                    },
                })
            })
        }
    }

    struct TextDetector;

    impl FormatDetector for TextDetector {
        fn id(&self) -> &'static str {
            "recovery.text"
        }

        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async { Ok(vec![FormatCandidate::new(InputFormat::Text, 1.0, "test")]) })
        }
    }

    struct CountingConverter(Arc<AtomicUsize>);

    struct MediaCheckpointingConverter {
        fail_once: Arc<AtomicBool>,
        recovered: Arc<AtomicBool>,
    }

    struct CountingTitleEnricher(Arc<AtomicUsize>);

    struct ReferencedAssetDroppingEnricher(Arc<AtomicUsize>);

    struct InvalidDiagnosticEnricher(Arc<AtomicUsize>);

    impl OutputEnricher for ReferencedAssetDroppingEnricher {
        fn id(&self) -> &'static str {
            "recovery.referenced-asset-dropping-enricher"
        }

        fn planned_enrichment_bytes(
            &self,
            _: &ConverterOutput,
            _: &str,
            _: InputFormat,
            _: &ConversionOptions,
            _: &Services,
            _: &ExecutionContext,
        ) -> Result<into_markdown_core::EnrichmentPlan, ConversionError> {
            Ok(into_markdown_core::EnrichmentPlan::Reserve(64 * 1024))
        }

        fn enrich<'a>(
            &'a self,
            mut output: ConverterOutput,
            _: &'a str,
            _: InputFormat,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            output.assets.clear();
            Box::pin(async move { Ok(output) })
        }
    }

    impl OutputEnricher for InvalidDiagnosticEnricher {
        fn id(&self) -> &'static str {
            "recovery.invalid-diagnostic-enricher"
        }

        fn planned_enrichment_bytes(
            &self,
            _: &ConverterOutput,
            _: &str,
            _: InputFormat,
            _: &ConversionOptions,
            _: &Services,
            _: &ExecutionContext,
        ) -> Result<into_markdown_core::EnrichmentPlan, ConversionError> {
            Ok(into_markdown_core::EnrichmentPlan::Reserve(64 * 1024))
        }

        fn enrich<'a>(
            &'a self,
            mut output: ConverterOutput,
            _: &'a str,
            _: InputFormat,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            output.diagnostics.push(Diagnostic {
                code: String::new(),
                severity: into_markdown_core::DiagnosticSeverity::Warning,
                message: "invalid diagnostic".into(),
                locator: None,
            });
            Box::pin(async move { Ok(output) })
        }
    }

    impl OutputEnricher for CountingTitleEnricher {
        fn id(&self) -> &'static str {
            "recovery.title-enricher"
        }

        fn planned_enrichment_bytes(
            &self,
            _: &ConverterOutput,
            _: &str,
            _: InputFormat,
            _: &ConversionOptions,
            _: &Services,
            _: &ExecutionContext,
        ) -> Result<into_markdown_core::EnrichmentPlan, ConversionError> {
            Ok(into_markdown_core::EnrichmentPlan::Reserve(64 * 1024))
        }

        fn enrich<'a>(
            &'a self,
            mut output: ConverterOutput,
            converter_id: &'a str,
            format: InputFormat,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(converter_id, "recovery.converter");
                assert_eq!(format, InputFormat::Text);
                output.document.metadata.title.get_or_insert_default().push_str(":enriched");
                Ok(output)
            })
        }
    }

    impl Converter for CountingConverter {
        fn id(&self) -> &'static str {
            "recovery.converter"
        }

        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }

        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }

        fn planned_output_bytes(
            &self,
            _: &ResolvedInput,
            _: &FormatCandidate,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(64 * 1024)
        }
        fn convert<'a>(
            &'a self,
            input: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let title = String::from_utf8_lossy(&input.bytes).into_owned();
            Box::pin(async move {
                let mut document = Document::default();
                document.metadata.title = Some(title);
                Ok(ConverterOutput::new(document, Vec::new(), Vec::new()))
            })
        }
    }

    impl Converter for MediaCheckpointingConverter {
        fn id(&self) -> &'static str {
            "recovery.media-checkpointing-converter"
        }

        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }

        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }

        fn planned_output_bytes(
            &self,
            _: &ResolvedInput,
            _: &FormatCandidate,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(256 * 1024)
        }

        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            Box::pin(async move {
                if let Some(checkpoint) = context.load_media_checkpoint()? {
                    let checkpoint = checkpoint.into_state();
                    assert_eq!(checkpoint.next_window_start_frame, 100);
                    self.recovered.store(true, Ordering::SeqCst);
                } else {
                    let range = TimeRange { start_ms: 0, end_ms: 1_000 };
                    context.commit_media_checkpoint(&MediaCheckpoint {
                        schema_version: MEDIA_CHECKPOINT_SCHEMA_VERSION,
                        audio: NormalizedAudioIdentity {
                            sha256: "a".repeat(64),
                            frames: 1_000,
                            sample_rate: 16_000,
                            channels: 1,
                        },
                        stage: MediaCheckpointStage::Transcribing,
                        next_window_start_frame: 100,
                        segments: vec![BlockNode {
                            id: into_markdown_core::NodeId("media-1".into()),
                            block: Block::TimedSegment {
                                range,
                                speaker: None,
                                speaker_confidence: None,
                                tokens: Vec::new(),
                                content: vec![Inline::Text {
                                    value: "partial".into(),
                                    marks: Vec::new(),
                                }],
                            },
                            provenance: Provenance {
                                kind: into_markdown_core::ProvenanceKind::AiProvider,
                                provider: "test/model".into(),
                                locator: SourceLocator {
                                    time: Some(range),
                                    ..SourceLocator::default()
                                },
                                confidence: Some(0.9),
                            },
                        }],
                        transcriber_provider: "test".into(),
                        transcriber_model: "model@sha256:abcd".into(),
                        language: Some("en".into()),
                        language_confidence: Some(0.9),
                        diarizer_provider: None,
                        diarization_model: None,
                        diarization_completed_segments: 0,
                        speaker_clusters: Vec::new(),
                    })?;
                }
                if self.fail_once.swap(false, Ordering::SeqCst) {
                    return Err(ConversionError::Io { detail: "injected media crash".into() });
                }
                let mut document = Document::default();
                document.metadata.title = Some("resumed media".into());
                Ok(ConverterOutput::new(document, Vec::new(), Vec::new()))
            })
        }
    }

    struct ControlledRenderer {
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    struct OversizedPlanRenderer {
        calls: Arc<AtomicUsize>,
    }

    struct MeasuredPlanRenderer {
        plan: u64,
        markdown: String,
        calls: Arc<AtomicUsize>,
        observed: Arc<std::sync::Mutex<Option<ExecutionContext>>>,
    }

    struct UniqueConverter(Arc<AtomicUsize>);

    struct FixtureConverter {
        conversions: Arc<AtomicUsize>,
        document: Document,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
    }

    impl Converter for UniqueConverter {
        fn id(&self) -> &'static str {
            "recovery.unique"
        }

        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }

        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }

        fn planned_output_bytes(
            &self,
            _: &ResolvedInput,
            _: &FormatCandidate,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(64 * 1024)
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            let sequence = self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let mut document = Document::default();
                document.metadata.title = Some(format!("winner-{sequence}"));
                Ok(ConverterOutput::new(document, Vec::new(), Vec::new()))
            })
        }
    }

    impl Converter for FixtureConverter {
        fn id(&self) -> &'static str {
            "recovery.fixture"
        }

        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }

        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }

        fn planned_output_bytes(
            &self,
            _: &ResolvedInput,
            _: &FormatCandidate,
            _: &ConversionOptions,
            context: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(context.available_memory_bytes())
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.conversions.fetch_add(1, Ordering::SeqCst);
            let output = ConverterOutput::new(
                self.document.clone(),
                self.assets.clone(),
                self.diagnostics.clone(),
            );
            Box::pin(async move { Ok(output) })
        }
    }

    impl MarkdownRenderer for ControlledRenderer {
        fn id(&self) -> &'static str {
            "recovery.renderer"
        }

        fn planned_markdown_bytes(
            &self,
            document: &Document,
            _: &[Asset],
            _: &ConversionOptions,
            context: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            let _ = document;
            Ok(context.available_memory_bytes())
        }
        fn render<'a>(
            &'a self,
            document: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = self.fail.swap(false, Ordering::SeqCst);
            let title = document.metadata.title.clone().unwrap_or_default();
            Box::pin(async move {
                if fail {
                    Err(ConversionError::Internal { detail: "injected renderer crash".into() })
                } else {
                    Ok(format!("# {title}\n"))
                }
            })
        }
    }

    impl MarkdownRenderer for OversizedPlanRenderer {
        fn id(&self) -> &'static str {
            "recovery.oversized-plan-renderer"
        }

        fn planned_markdown_bytes(
            &self,
            _: &Document,
            _: &[Asset],
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(80_000)
        }

        fn render<'a>(
            &'a self,
            _: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(String::new()) })
        }
    }

    impl MarkdownRenderer for MeasuredPlanRenderer {
        fn id(&self) -> &'static str {
            "recovery.measured-plan-renderer"
        }

        fn planned_markdown_bytes(
            &self,
            _: &Document,
            _: &[Asset],
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(self.plan)
        }

        fn render<'a>(
            &'a self,
            _: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.observed.lock().unwrap() = Some(context.clone());
            let markdown = self.markdown.clone();
            Box::pin(async move { Ok(markdown) })
        }
    }

    fn engine_with_renderer(
        conversions: Arc<AtomicUsize>,
        renderer: Arc<dyn MarkdownRenderer>,
    ) -> Engine {
        let mut builder = EngineBuilder::new().renderer(renderer);
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(CountingConverter(conversions)));
        builder.build().unwrap()
    }

    fn engine(
        conversions: Arc<AtomicUsize>,
        renderer_calls: Arc<AtomicUsize>,
        fail_renderer: Arc<AtomicBool>,
    ) -> Engine {
        let mut builder = EngineBuilder::new()
            .renderer(Arc::new(ControlledRenderer { fail: fail_renderer, calls: renderer_calls }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(CountingConverter(conversions)));
        builder.build().unwrap()
    }

    fn enriched_engine(
        conversions: Arc<AtomicUsize>,
        enrichments: Arc<AtomicUsize>,
        renderer_calls: Arc<AtomicUsize>,
        fail_renderer: Arc<AtomicBool>,
    ) -> Engine {
        let mut builder = EngineBuilder::new()
            .enricher(Arc::new(CountingTitleEnricher(enrichments)))
            .renderer(Arc::new(ControlledRenderer { fail: fail_renderer, calls: renderer_calls }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(CountingConverter(conversions)));
        builder.build().unwrap()
    }

    fn unique_engine(conversions: Arc<AtomicUsize>) -> Engine {
        let mut builder = EngineBuilder::new().renderer(Arc::new(ControlledRenderer {
            fail: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(UniqueConverter(conversions)));
        builder.build().unwrap()
    }

    fn fixture_engine(
        output: &ConverterOutput,
        conversions: Arc<AtomicUsize>,
        renderer_calls: Arc<AtomicUsize>,
        fail_renderer: Arc<AtomicBool>,
    ) -> Engine {
        let mut builder = EngineBuilder::new()
            .renderer(Arc::new(ControlledRenderer { fail: fail_renderer, calls: renderer_calls }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(FixtureConverter {
                conversions,
                document: output.document.clone(),
                assets: output.assets.clone(),
                diagnostics: output.diagnostics.clone(),
            }));
        builder.build().unwrap()
    }

    fn fixture_engine_with_enricher(
        output: &ConverterOutput,
        conversions: Arc<AtomicUsize>,
        renderer_calls: Arc<AtomicUsize>,
        enricher: Arc<dyn OutputEnricher>,
    ) -> Engine {
        let mut builder =
            EngineBuilder::new().enricher(enricher).renderer(Arc::new(ControlledRenderer {
                fail: Arc::new(AtomicBool::new(false)),
                calls: renderer_calls,
            }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(FixtureConverter {
                conversions,
                document: output.document.clone(),
                assets: output.assets.clone(),
                diagnostics: output.diagnostics.clone(),
            }));
        builder.build().unwrap()
    }

    fn fixture_provenance() -> Provenance {
        Provenance {
            kind: into_markdown_core::ProvenanceKind::NativeParser,
            provider: "recovery.fixture".into(),
            locator: SourceLocator::default(),
            confidence: Some(1.0),
        }
    }

    fn fixture_node(id: impl Into<String>, block: Block) -> BlockNode {
        BlockNode {
            id: into_markdown_core::NodeId(id.into()),
            block,
            provenance: fixture_provenance(),
        }
    }

    fn maximum_depth_document(extra_depth: usize) -> Document {
        use into_markdown_core::{Cell, TableRow};

        let mut nested = fixture_node("depth-0", Block::Rule);
        for level in 1..into_markdown_core::MAX_DOCUMENT_DEPTH + extra_depth {
            nested = fixture_node(
                format!("depth-{level}"),
                Block::Table {
                    rows: vec![TableRow {
                        cells: vec![Cell {
                            row_span: 1,
                            column_span: 1,
                            header: false,
                            blocks: vec![nested],
                        }],
                    }],
                    alignments: Vec::new(),
                },
            );
        }
        let mut document = Document { blocks: vec![nested], ..Document::default() };
        document.metadata.title = Some("maximum depth".into());
        document
    }

    fn assert_result_matches_fixture(result: &ConversionResult, fixture: &ConverterOutput) {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let (provenance, _memory) =
            collect_provenance_preflighted(&fixture.document.blocks, &context).unwrap();
        assert_eq!(result.document, fixture.document);
        assert_eq!(result.assets, fixture.assets);
        assert_eq!(result.diagnostics, fixture.diagnostics);
        assert_eq!(result.provenance, provenance);
        assert_eq!(
            result.markdown,
            format!("# {}\n", fixture.document.metadata.title.as_deref().unwrap_or_default())
        );
    }

    fn assert_fixture_recovers_converted_and_succeeded(fixture: ConverterOutput) {
        let directory = private_tempdir();
        let token = RecoveryToken::parse("00112233445566778899aabbccddeeff").unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let renders = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(true));
        let request = || ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));

        {
            let store = RecoveryStore::open(directory.path()).unwrap();
            let engine = fixture_engine(
                &fixture,
                Arc::clone(&conversions),
                Arc::clone(&renders),
                Arc::clone(&fail),
            );
            let error =
                block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Converted);
        }
        {
            let store = RecoveryStore::open(directory.path()).unwrap();
            let engine = fixture_engine(
                &fixture,
                Arc::clone(&conversions),
                Arc::clone(&renders),
                Arc::clone(&fail),
            );
            let result = block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
            assert_result_matches_fixture(&result, &fixture);
            assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);
        }
        {
            let store = RecoveryStore::open(directory.path()).unwrap();
            let engine = fixture_engine(
                &fixture,
                Arc::clone(&conversions),
                Arc::clone(&renders),
                Arc::clone(&fail),
            );
            let result = block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
            assert_result_matches_fixture(&result, &fixture);
        }
        assert_eq!(conversions.load(Ordering::SeqCst), 1);
        assert_eq!(renders.load(Ordering::SeqCst), 3);
    }

    fn checkpoint_asset_json(asset: &Asset) -> serde_json::Value {
        serde_json::json!({
            "id": asset.id,
            "filename": asset.filename,
            "mediaType": asset.media_type,
            "decodedBytes": asset.bytes.len(),
            "dataBase64": STANDARD.encode(&asset.bytes),
            "externalUri": asset.external_uri,
        })
    }

    fn checkpoint_request(max_memory_bytes: u64) -> ConversionRequest {
        let mut request = ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));
        request.options.limits.max_memory_bytes = max_memory_bytes;
        request
    }

    fn commit_fixture_checkpoint(
        store: &RecoveryStore,
        token: &RecoveryToken,
        phase: TaskPhase,
        fixture: &ConverterOutput,
        markdown: &str,
        provenance: &[Provenance],
        request: &ConversionRequest,
    ) {
        let context =
            ExecutionContext::new(ExecutionOptions::default(), request.options.limits.clone());
        let metadata =
            SourceMetadata { name: Some("x".into()), size: 7, ..SourceMetadata::default() };
        let input_fingerprint = fingerprint_input(b"fixture", &metadata).unwrap();
        let options_fingerprint =
            fingerprint_json(&(request.hint.clone(), request.options.clone())).unwrap();
        match phase {
            TaskPhase::Media => panic!("fixture helper does not create media checkpoints"),
            TaskPhase::Converted => store
                .commit(
                    token,
                    &context,
                    &input_fingerprint,
                    &options_fingerprint,
                    phase,
                    &CheckpointPayloadRef::Converted {
                        document: &fixture.document,
                        assets: CheckpointAssetsRef(&fixture.assets),
                        diagnostics: &fixture.diagnostics,
                    },
                )
                .unwrap(),
            TaskPhase::Succeeded => store
                .commit(
                    token,
                    &context,
                    &input_fingerprint,
                    &options_fingerprint,
                    phase,
                    &CheckpointPayloadRef::Succeeded {
                        document: &fixture.document,
                        markdown,
                        assets: CheckpointAssetsRef(&fixture.assets),
                        diagnostics: &fixture.diagnostics,
                        provenance,
                    },
                )
                .unwrap(),
        }
    }

    fn decoded_checkpoint_reservation(
        store: &RecoveryStore,
        token: &RecoveryToken,
        request: &ConversionRequest,
    ) -> u64 {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 64 * 1024 * 1024, ..Default::default() },
        );
        let store::LoadedCheckpoint { payload, mut memory, .. } =
            store.load::<CheckpointPayloadWire>(token, &context).unwrap().unwrap();
        let _payload = payload.decode(&context, &mut memory, request).unwrap();
        context.reserved_memory_bytes()
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[cfg(unix)]
    fn set_directory_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn private_tempdir() -> tempfile::TempDir {
        let directory =
            tempfile::Builder::new().prefix("into-markdown-recovery-").tempdir().unwrap();
        set_directory_mode(directory.path(), 0o700);
        directory
    }

    #[cfg(unix)]
    fn assert_unsafe_recovery<T>(result: Result<T, ConversionError>) {
        let Err(error) = result else { panic!("unsafe recovery root was accepted") };
        assert!(matches!(error, ConversionError::Recovery { reason: "unsafePath", .. }), "{error}");
    }

    #[test]
    fn enricher_cannot_publish_checkpoint_with_missing_referenced_asset() {
        let asset_id = AssetId("referenced-image".into());
        let fixture = ConverterOutput::new(
            Document {
                blocks: vec![fixture_node(
                    "image-node",
                    Block::Image { asset: asset_id.clone(), alt: Some("diagram".into()) },
                )],
                ..Document::default()
            },
            vec![Asset {
                id: asset_id,
                filename: Some("diagram.png".into()),
                media_type: "image/png".into(),
                bytes: vec![1],
                external_uri: None,
            }],
            Vec::new(),
        );
        let directory = private_tempdir();
        let token = RecoveryToken::parse("11223344556677889900aabbccddeeff").unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let enrichments = Arc::new(AtomicUsize::new(0));
        let renders = Arc::new(AtomicUsize::new(0));
        let request = || ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));

        for expected_attempts in 1..=2 {
            let store = RecoveryStore::open(directory.path()).unwrap();
            let engine = fixture_engine_with_enricher(
                &fixture,
                Arc::clone(&conversions),
                Arc::clone(&renders),
                Arc::new(ReferencedAssetDroppingEnricher(Arc::clone(&enrichments))),
            );
            let error =
                block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
            assert!(
                matches!(error, ConversionError::Recovery { reason: "corrupt", .. }),
                "{error}"
            );
            assert!(error.to_string().contains("missing asset referenced-image"), "{error}");
            assert!(store.inspect(&token).unwrap().is_none());
            assert_eq!(conversions.load(Ordering::SeqCst), expected_attempts);
            assert_eq!(enrichments.load(Ordering::SeqCst), expected_attempts);
            assert_eq!(renders.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn enricher_cannot_publish_checkpoint_with_invalid_diagnostic() {
        let fixture = ConverterOutput::new(Document::default(), Vec::new(), Vec::new());
        let directory = private_tempdir();
        let token = RecoveryToken::parse("223344556677889900aabbccddeeff11").unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let enrichments = Arc::new(AtomicUsize::new(0));
        let renders = Arc::new(AtomicUsize::new(0));
        let request = || ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));

        for expected_attempts in 1..=2 {
            let store = RecoveryStore::open(directory.path()).unwrap();
            let engine = fixture_engine_with_enricher(
                &fixture,
                Arc::clone(&conversions),
                Arc::clone(&renders),
                Arc::new(InvalidDiagnosticEnricher(Arc::clone(&enrichments))),
            );
            let error =
                block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
            assert!(
                matches!(error, ConversionError::Recovery { reason: "corrupt", .. }),
                "{error}"
            );
            assert!(error.to_string().contains("diagnostic code"), "{error}");
            assert!(store.inspect(&token).unwrap().is_none());
            assert_eq!(conversions.load(Ordering::SeqCst), expected_attempts);
            assert_eq!(enrichments.load(Ordering::SeqCst), expected_attempts);
            assert_eq!(renders.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn process_restart_resumes_converted_phase_idempotently() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let renders = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(true));
        let request = || ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x")));

        let first = engine(Arc::clone(&conversions), Arc::clone(&renders), Arc::clone(&fail));
        assert_eq!(
            block_on(first.convert_recoverable(request(), &store, &token)).unwrap_err().code(),
            into_markdown_core::ErrorCode::Internal
        );
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Converted);

        let restarted = engine(Arc::clone(&conversions), Arc::clone(&renders), Arc::clone(&fail));
        let result = block_on(restarted.convert_recoverable(request(), &store, &token)).unwrap();
        assert_eq!(result.markdown, "# hello\n");
        assert_eq!(conversions.load(Ordering::SeqCst), 1);
        assert_eq!(renders.load(Ordering::SeqCst), 2);
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);

        let replay = block_on(restarted.convert_recoverable(request(), &store, &token)).unwrap();
        assert_eq!(replay.markdown, result.markdown);
        assert_eq!(conversions.load(Ordering::SeqCst), 1);
        assert_eq!(renders.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn purge_quarantine_post_move_failures_restore_live_names_and_reopen() {
        for failure in 1..=2 {
            let directory = private_tempdir();
            let store = RecoveryStore::open(directory.path()).unwrap();
            let token = store.create_token().unwrap();
            let conversions = Arc::new(AtomicUsize::new(0));
            let renders = Arc::new(AtomicUsize::new(0));
            let fail = Arc::new(AtomicBool::new(false));
            let converter = engine(conversions, renders, fail);
            let request =
                || ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x")));
            block_on(converter.convert_recoverable(request(), &store, &token)).unwrap();
            assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);

            store.test_fail_quarantine_after_move(failure);
            assert!(store.quarantine_purge(&token).is_err());
            assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);
            drop(store);

            let reopened = RecoveryStore::open(directory.path()).unwrap();
            assert_eq!(reopened.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);
        }
    }

    #[test]
    fn enriched_output_is_checkpointed_transactionally_and_not_reenriched_after_restart() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let enrichments = Arc::new(AtomicUsize::new(0));
        let renders = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(true));
        let request = || ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x")));

        let first = enriched_engine(
            Arc::clone(&conversions),
            Arc::clone(&enrichments),
            Arc::clone(&renders),
            Arc::clone(&fail),
        );
        assert_eq!(
            block_on(first.convert_recoverable(request(), &store, &token)).unwrap_err().code(),
            ErrorCode::Internal
        );
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Converted);

        let restarted = enriched_engine(
            Arc::clone(&conversions),
            Arc::clone(&enrichments),
            Arc::clone(&renders),
            Arc::clone(&fail),
        );
        let result = block_on(restarted.convert_recoverable(request(), &store, &token)).unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("hello:enriched"));
        assert_eq!(result.markdown, "# hello:enriched\n");
        assert_eq!(conversions.load(Ordering::SeqCst), 1);
        assert_eq!(enrichments.load(Ordering::SeqCst), 1);
        assert_eq!(renders.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn recovery_renderer_preflight_rejects_converted_and_succeeded_before_render_call() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let converted_token = store.create_token().unwrap();
        let succeeded_token = store.create_token().unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let renderer_calls = Arc::new(AtomicUsize::new(0));
        let request = || {
            let mut request =
                ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x")));
            request.options.limits.max_memory_bytes = 70_000;
            request
        };

        let fail = Arc::new(AtomicBool::new(true));
        let initial =
            engine(Arc::clone(&conversions), Arc::new(AtomicUsize::new(0)), Arc::clone(&fail));
        assert_eq!(
            block_on(initial.convert_recoverable(request(), &store, &converted_token))
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(store.inspect(&converted_token).unwrap().unwrap().phase, TaskPhase::Converted);

        fail.store(false, Ordering::SeqCst);
        block_on(initial.convert_recoverable(request(), &store, &succeeded_token)).unwrap();
        assert_eq!(store.inspect(&succeeded_token).unwrap().unwrap().phase, TaskPhase::Succeeded);

        let rejecting = engine_with_renderer(
            Arc::clone(&conversions),
            Arc::new(OversizedPlanRenderer { calls: Arc::clone(&renderer_calls) }),
        );
        for token in [&converted_token, &succeeded_token] {
            assert_eq!(
                block_on(rejecting.convert_recoverable(request(), &store, token))
                    .unwrap_err()
                    .code(),
                ErrorCode::ResourceLimit
            );
        }
        assert_eq!(renderer_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn recovered_decode_lease_shrinks_to_retained_before_renderer_for_both_phases() {
        const RENDERER_PLAN: u64 = 8 * 1024 * 1024;
        let mut document = Document::default();
        document.metadata.title = Some("lease".into());
        let fixture = ConverterOutput::new(
            document,
            vec![Asset {
                id: AssetId("payload".into()),
                filename: Some("payload.bin".into()),
                media_type: "application/octet-stream".into(),
                bytes: vec![0x5a; 1_200_000],
                external_uri: None,
            }],
            Vec::new(),
        );
        let markdown = "# lease\n".to_owned();
        let provenance = Vec::new();
        let output_retained = into_markdown_core::estimate_retained_output(
            &fixture.document,
            &fixture.assets,
            &fixture.diagnostics,
        )
        .unwrap();
        let result_retained = estimate_retained_result(
            &fixture.document,
            &markdown,
            &fixture.assets,
            &fixture.diagnostics,
            &provenance,
        )
        .unwrap();
        let source_charge = 7_u64;

        for (phase, retained) in
            [(TaskPhase::Converted, output_retained), (TaskPhase::Succeeded, result_retained)]
        {
            for exact in [true, false] {
                let directory = private_tempdir();
                let store = RecoveryStore::open(directory.path()).unwrap();
                let token = store.create_token().unwrap();
                let exact_limit = source_charge
                    .checked_add(retained)
                    .and_then(|value| value.checked_add(RENDERER_PLAN))
                    .unwrap();
                let limit = exact_limit - u64::from(!exact);
                let request = checkpoint_request(limit);
                commit_fixture_checkpoint(
                    &store,
                    &token,
                    phase,
                    &fixture,
                    &markdown,
                    &provenance,
                    &request,
                );
                let decoded = decoded_checkpoint_reservation(&store, &token, &request);
                assert!(decoded > retained);
                assert!(
                    source_charge + retained + RENDERER_PLAN <= exact_limit
                        && exact_limit < source_charge + decoded + RENDERER_PLAN
                );

                let calls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::new(std::sync::Mutex::new(None));
                let renderer = Arc::new(MeasuredPlanRenderer {
                    plan: RENDERER_PLAN,
                    markdown: markdown.clone(),
                    calls: Arc::clone(&calls),
                    observed: Arc::clone(&observed),
                });
                let engine = engine_with_renderer(Arc::new(AtomicUsize::new(0)), renderer);
                let recovered = block_on(engine.convert_recoverable(request, &store, &token));
                if exact {
                    let result = recovered.unwrap();
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                    let context = observed.lock().unwrap().take().unwrap();
                    let final_retained = estimate_retained_result(
                        &result.document,
                        &result.markdown,
                        &result.assets,
                        &result.diagnostics,
                        &result.provenance,
                    )
                    .unwrap();
                    assert_eq!(context.reserved_memory_bytes(), final_retained);
                    drop(result);
                    assert_eq!(context.reserved_memory_bytes(), 0);
                } else {
                    assert!(matches!(
                        recovered,
                        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
                    ));
                    assert_eq!(calls.load(Ordering::SeqCst), 0);
                    assert!(observed.lock().unwrap().is_none());
                }
            }
        }
    }

    #[test]
    fn canonical_binary_asset_over_old_json_width_round_trips_every_phase() {
        let mut document = Document::default();
        document.metadata.title = Some("large asset".into());
        let fixture = ConverterOutput::new(
            document,
            vec![Asset {
                id: AssetId("large".into()),
                filename: Some("large.bin".into()),
                media_type: "application/octet-stream".into(),
                bytes: vec![0xa5; 1_200_000],
                external_uri: None,
            }],
            Vec::new(),
        );
        assert_fixture_recovers_converted_and_succeeded(fixture);
    }

    #[test]
    fn maximum_public_ir_depth_round_trips_every_phase() {
        let document = maximum_depth_document(0);
        document.validate().unwrap();
        assert_fixture_recovers_converted_and_succeeded(ConverterOutput::new(
            document,
            Vec::new(),
            Vec::new(),
        ));
    }

    #[test]
    fn source_text_round_trips_every_recovery_phase() {
        let mut document = Document::default();
        document.metadata.title = Some("source text".into());
        document.blocks.push(fixture_node(
            "source",
            Block::Paragraph(vec![Inline::SourceText {
                value: "字".into(),
                marks: vec![],
                provenance: Box::new(Provenance {
                    kind: into_markdown_core::ProvenanceKind::NativeParser,
                    provider: "pdfium".into(),
                    locator: SourceLocator {
                        page: Some(1),
                        character_index: Some(9),
                        ..SourceLocator::default()
                    },
                    confidence: None,
                }),
            }]),
        ));
        assert_fixture_recovers_converted_and_succeeded(ConverterOutput::new(
            document,
            Vec::new(),
            Vec::new(),
        ));
    }

    #[test]
    fn changed_input_and_configuration_reject_old_checkpoint() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let engine = engine(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let request = ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
        block_on(engine.convert_recoverable(request, &store, &token)).unwrap();

        let changed = ConversionRequest::new(InputRef::bytes(b"two".as_slice(), Some("x")));
        let error = block_on(engine.convert_recoverable(changed, &store, &token)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
        assert!(error.to_string().contains("incompatible"));

        let mut changed_options =
            ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
        changed_options.options.output.include_provenance = false;
        let error =
            block_on(engine.convert_recoverable(changed_options, &store, &token)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
        assert!(error.to_string().contains("configuration changed"));
    }

    #[test]
    fn media_chunk_checkpoint_resumes_converter_and_is_removed_after_converted_commit() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let fail_once = Arc::new(AtomicBool::new(true));
        let recovered = Arc::new(AtomicBool::new(false));
        let build = || {
            let mut builder = EngineBuilder::new().renderer(Arc::new(ControlledRenderer {
                fail: Arc::new(AtomicBool::new(false)),
                calls: Arc::new(AtomicUsize::new(0)),
            }));
            builder
                .registry_mut()
                .register_source_resolver(Arc::new(BytesResolver))
                .register_format_detector(Arc::new(TextDetector))
                .register_converter(Arc::new(MediaCheckpointingConverter {
                    fail_once: Arc::clone(&fail_once),
                    recovered: Arc::clone(&recovered),
                }));
            builder.build().unwrap()
        };
        let media_request =
            || ConversionRequest::new(InputRef::bytes(b"media".as_slice(), Some("meeting.txt")));
        let first =
            block_on(build().convert_recoverable(media_request(), &store, &token)).unwrap_err();
        assert_eq!(first.code(), ErrorCode::Io);
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Media);

        let result =
            block_on(build().convert_recoverable(media_request(), &store, &token)).unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("resumed media"));
        assert!(recovered.load(Ordering::SeqCst));
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);
        assert!(!store.test_path(&token, TaskPhase::Media).exists());
    }

    #[test]
    fn interrupted_or_corrupt_writes_never_look_successful() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = RecoveryToken::parse("00112233445566778899aabbccddeeff").unwrap();
        fs::write(
            directory.path().join(format!(".{}.crash.tmp", token.as_str())),
            br#"{"schemaVersion":1,"phase":"succeeded"}"#,
        )
        .unwrap();
        assert!(store.inspect(&token).unwrap().is_none());

        fs::write(
            store.test_path(&token, TaskPhase::Succeeded),
            br#"{"schemaVersion":1,"phase":"succeeded""#,
        )
        .unwrap();
        let error = store.inspect(&token).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
        assert!(error.to_string().contains("corrupt"));
    }

    #[test]
    fn checkpoint_write_crash_child() {
        let Ok(root) = std::env::var("INTO_MARKDOWN_RECOVERY_CRASH_ROOT") else {
            return;
        };
        let token = "00112233445566778899aabbccddeeff";
        let path = Path::new(&root).join(format!(".{token}.injected.tmp"));
        let mut file = File::create(path).unwrap();
        file.write_all(br#"{"schemaVersion":1,"phase":"succeeded""#).unwrap();
        file.sync_all().unwrap();
        std::process::abort();
    }

    #[test]
    fn real_process_crash_leaves_no_false_success() {
        let directory = private_tempdir();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recovery::tests::checkpoint_write_crash_child"])
            .env("INTO_MARKDOWN_RECOVERY_CRASH_ROOT", directory.path())
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(!status.success());

        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = RecoveryToken::parse("00112233445566778899aabbccddeeff").unwrap();
        assert!(store.inspect(&token).unwrap().is_none());
    }

    #[test]
    fn token_and_checkpoint_version_are_strictly_compatible() {
        assert!(RecoveryToken::parse("../escape").is_err());
        assert!(RecoveryToken::parse("00112233445566778899AABBCCDDEEFF").is_err());

        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let engine = engine(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let request = ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
        block_on(engine.convert_recoverable(request, &store, &token)).unwrap();
        let metadata = store.inspect(&token).unwrap().unwrap();
        let payload = store.test_payload(&token, TaskPhase::Succeeded);
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            2,
            &metadata.input_fingerprint,
            &metadata.options_fingerprint,
            &payload,
        );
        let error = store.inspect(&token).unwrap_err();
        assert!(error.to_string().contains("unsupportedVersion"));
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_symlink_is_rejected_without_reading_its_target() {
        use std::os::unix::fs::symlink;

        let directory = private_tempdir();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), br#"{"secret":"must not be decoded"}"#).unwrap();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = RecoveryToken::parse("00112233445566778899aabbccddeeff").unwrap();
        symlink(outside.path(), store.test_path(&token, TaskPhase::Succeeded)).unwrap();

        let error = store.inspect(&token).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
        assert!(error.to_string().contains("unsafePath"));
    }

    #[cfg(unix)]
    #[test]
    fn final_store_root_requires_private_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        for mode in [0o777, 0o750] {
            let parent = private_tempdir();
            let root = parent.path().join("existing-store");
            fs::create_dir(&root).unwrap();
            set_directory_mode(&root, mode);
            assert_unsafe_recovery(RecoveryStore::open(&root));
            assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777, mode);
        }

        let parent = private_tempdir();
        set_directory_mode(parent.path(), 0o777);
        let existing = parent.path().join("private-store");
        fs::create_dir(&existing).unwrap();
        set_directory_mode(&existing, 0o700);
        RecoveryStore::open(&existing).unwrap();

        let created = parent.path().join("created-store");
        RecoveryStore::open(&created).unwrap();
        assert_eq!(fs::metadata(created).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn chmod_after_open_makes_every_store_operation_fail_closed() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = RecoveryToken::parse("00112233445566778899aabbccddeeff").unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let expose = || set_directory_mode(directory.path(), 0o750);
        let privatize = || set_directory_mode(directory.path(), 0o700);

        expose();
        assert_unsafe_recovery(store.create_token());
        privatize();

        expose();
        assert_unsafe_recovery(store.inspect(&token));
        privatize();

        expose();
        assert_unsafe_recovery(store.lock(&token, &context));
        privatize();

        expose();
        assert_unsafe_recovery(store.load::<CheckpointPayloadWire>(&token, &context));
        privatize();

        expose();
        assert_unsafe_recovery(store.commit(
            &token,
            &context,
            TEST_INPUT_FINGERPRINT,
            TEST_OPTIONS_FINGERPRINT,
            TaskPhase::Converted,
            &serde_json::json!({"kind": "converted"}),
        ));
        privatize();

        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn root_and_ancestor_swaps_cannot_redirect_checkpoint_io() {
        use std::os::unix::fs::symlink;

        for swap_root in [true, false] {
            let directory = private_tempdir();
            let ancestor = directory.path().join("ancestor");
            let root = ancestor.join("store");
            fs::create_dir_all(&root).unwrap();
            set_directory_mode(&root, 0o700);
            let store = RecoveryStore::open(&root).unwrap();
            let token = store.create_token().unwrap();
            let outside = directory.path().join("outside");
            fs::create_dir(&outside).unwrap();
            if swap_root {
                fs::rename(&root, ancestor.join("held-store")).unwrap();
                symlink(&outside, &root).unwrap();
            } else {
                fs::rename(&ancestor, directory.path().join("held-ancestor")).unwrap();
                symlink(&outside, &ancestor).unwrap();
            }
            let error = store.inspect(&token).unwrap_err();
            assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
            let engine = engine(
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
            );
            let request = ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
            let error = block_on(engine.convert_recoverable(request, &store, &token)).unwrap_err();
            assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery);
            assert!(fs::read_dir(&outside).unwrap().next().is_none());
        }
    }

    #[test]
    fn concurrent_same_token_returns_the_single_persisted_winner() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let conversions = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let engine = unique_engine(Arc::clone(&conversions));
            let store = store.clone();
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let request =
                    ConversionRequest::new(InputRef::bytes(b"same".as_slice(), Some("x")));
                block_on(engine.convert_recoverable(request, &store, &token)).unwrap()
            }));
        }
        barrier.wait();
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert_eq!(conversions.load(Ordering::SeqCst), 1);
        assert_eq!(first.markdown, second.markdown);
        assert_eq!(first.document, second.document);
        assert_eq!(first.assets, second.assets);
        assert_eq!(first.diagnostics, second.diagnostics);
        assert_eq!(first.provenance, second.provenance);
        assert_eq!(store.inspect(&token).unwrap().unwrap().phase, TaskPhase::Succeeded);
    }

    #[test]
    fn checkpoint_writes_obey_temporary_budget() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let engine = engine(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let mut request = ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
        request.options.limits.max_temporary_bytes = 1;
        let error = block_on(engine.convert_recoverable(request, &store, &token)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(store.inspect(&token).unwrap().is_none());
    }

    #[test]
    fn inspect_reads_only_fixed_metadata_footer_without_decoding_payload() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let payload = vec![b'x'; 8 * 1024 * 1024];
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            1,
            TEST_INPUT_FINGERPRINT,
            TEST_OPTIONS_FINGERPRINT,
            &payload,
        );
        let metadata = store.inspect(&token).unwrap().unwrap();
        assert_eq!(metadata.payload_bytes, payload.len() as u64);

        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let error = store.load::<CheckpointPayloadWire>(&token, &context).unwrap_err();
        assert!(error.to_string().contains("decode checkpoint payload"));
    }

    #[test]
    fn deeply_nested_checkpoint_is_rejected_before_typed_decode() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let excessive = store::MAX_CHECKPOINT_JSON_DEPTH + 1;
        let mut payload = vec![b'['; excessive];
        payload.extend(std::iter::repeat_n(b']', excessive));
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            1,
            TEST_INPUT_FINGERPRINT,
            TEST_OPTIONS_FINGERPRINT,
            &payload,
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let error = store.load::<CheckpointPayloadWire>(&token, &context).unwrap_err();
        assert!(error.to_string().contains("nesting is too deep"));
    }

    #[test]
    fn checkpoint_width_and_file_size_are_preflighted() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let mut payload = Vec::with_capacity(2_200_004);
        payload.push(b'[');
        for index in 0..1_100_001 {
            if index != 0 {
                payload.push(b',');
            }
            payload.push(b'0');
        }
        payload.push(b']');
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            1,
            TEST_INPUT_FINGERPRINT,
            TEST_OPTIONS_FINGERPRINT,
            &payload,
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let error = store.load::<CheckpointPayloadWire>(&token, &context).unwrap_err();
        assert!(error.to_string().contains("container is too wide"));

        let oversized = store.test_path(&token, TaskPhase::Succeeded);
        File::create(oversized).unwrap().set_len(2 * 1024 * 1024 * 1024 + 1).unwrap();
        let error = store.inspect(&token).unwrap_err();
        assert!(error.to_string().contains("2 GiB"));
    }

    #[test]
    fn damaged_noncanonical_or_length_mismatched_asset_base64_fails_closed() {
        for (malformed, declared, expected) in
            [("AQ=", 1, "base64"), ("AB==", 1, "base64"), ("AQ==", 2, "decoded length")]
        {
            let directory = private_tempdir();
            let store = RecoveryStore::open(directory.path()).unwrap();
            let token = store.create_token().unwrap();
            let mut document = Document::default();
            document.metadata.title = Some("base64".into());
            let fixture = ConverterOutput::new(
                document,
                vec![Asset {
                    id: AssetId("asset".into()),
                    filename: None,
                    media_type: "application/octet-stream".into(),
                    bytes: vec![1],
                    external_uri: None,
                }],
                Vec::new(),
            );
            let engine = fixture_engine(
                &fixture,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
            );
            let request =
                || ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));
            block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
            let metadata = store.inspect(&token).unwrap().unwrap();
            let mut payload: serde_json::Value =
                serde_json::from_slice(&store.test_payload(&token, TaskPhase::Succeeded)).unwrap();
            payload["value"]["assets"][0]["decodedBytes"] = serde_json::json!(declared);
            payload["value"]["assets"][0]["dataBase64"] = serde_json::json!(malformed);
            let payload = serde_json::to_vec(&payload).unwrap();
            store.test_replace_envelope(
                &token,
                TaskPhase::Succeeded,
                1,
                &metadata.input_fingerprint,
                &metadata.options_fingerprint,
                &payload,
            );
            let error =
                block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Recovery, "{malformed}");
            assert!(error.to_string().contains(expected), "{malformed}: {error}");
        }
    }

    #[test]
    fn decoded_asset_limit_plus_one_is_rejected_before_allocation() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let mut document = Document::default();
        document.metadata.title = Some("asset limit".into());
        let fixture = ConverterOutput::new(
            document,
            vec![Asset {
                id: AssetId("asset".into()),
                filename: None,
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                external_uri: None,
            }],
            Vec::new(),
        );
        let engine = fixture_engine(
            &fixture,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let request = || {
            let mut request =
                ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));
            request.options.limits.max_asset_bytes = 1;
            request.options.limits.max_total_asset_bytes = 1;
            request
        };
        block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
        let metadata = store.inspect(&token).unwrap().unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&store.test_payload(&token, TaskPhase::Succeeded)).unwrap();
        payload["value"]["assets"][0]["decodedBytes"] = serde_json::json!(2);
        payload["value"]["assets"][0]["dataBase64"] = serde_json::json!("AQI=");
        let payload = serde_json::to_vec(&payload).unwrap();
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            1,
            &metadata.input_fingerprint,
            &metadata.options_fingerprint,
            &payload,
        );
        let error = block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert!(error.to_string().contains("max_asset_bytes"));
    }

    #[test]
    fn public_document_depth_plus_one_is_rejected_on_resume() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let document = maximum_depth_document(0);
        document.validate().unwrap();
        let fixture = ConverterOutput::new(document, Vec::new(), Vec::new());
        let engine = fixture_engine(
            &fixture,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let request = || ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));
        block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
        let metadata = store.inspect(&token).unwrap().unwrap();
        let invalid = maximum_depth_document(1);
        assert!(invalid.validate().is_err());
        let mut payload: serde_json::Value =
            serde_json::from_slice(&store.test_payload(&token, TaskPhase::Succeeded)).unwrap();
        payload["value"]["document"] = serde_json::to_value(invalid).unwrap();
        let payload = serde_json::to_vec(&payload).unwrap();
        store.test_replace_envelope(
            &token,
            TaskPhase::Succeeded,
            1,
            &metadata.input_fingerprint,
            &metadata.options_fingerprint,
            &payload,
        );
        let error = block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Recovery);
        assert!(error.to_string().contains("invalid document IR"));
    }

    #[test]
    fn decoded_asset_and_typed_wire_share_the_request_memory_peak() {
        let directory = private_tempdir();
        let store = RecoveryStore::open(directory.path()).unwrap();
        let token = store.create_token().unwrap();
        let mut document = Document::default();
        document.metadata.title = Some("memory".into());
        let fixture = ConverterOutput::new(
            document,
            vec![Asset {
                id: AssetId("large".into()),
                filename: None,
                media_type: "application/octet-stream".into(),
                bytes: vec![0xa5; 1_200_000],
                external_uri: None,
            }],
            Vec::new(),
        );
        let engine = fixture_engine(
            &fixture,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let request = || {
            let mut request =
                ConversionRequest::new(InputRef::bytes(b"fixture".as_slice(), Some("x")));
            // The checkpoint envelope and owned base64 wire fit together, but
            // retaining both while allocating the decoded bytes does not.
            request.options.limits.max_memory_bytes = 4 * 1024 * 1024;
            request
        };
        block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
        let load_context =
            ExecutionContext::new(ExecutionOptions::default(), request().options.limits);
        let typed = store.load::<CheckpointPayloadWire>(&token, &load_context).unwrap().unwrap();
        assert!(matches!(typed.payload, CheckpointPayloadWire::Succeeded { .. }));
        drop(typed);
        let error = block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert!(error.to_string().contains("max_memory_bytes"));
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn tampered_success_payload_cannot_bypass_result_contract() {
        use into_markdown_core::{AssetId, ProvenanceKind, SourceLocator};

        for mutation in [
            "markdown",
            "asset-id",
            "asset-mime",
            "asset-uri",
            "asset-reference",
            "diagnostic",
            "provenance",
        ] {
            let directory = private_tempdir();
            let store = RecoveryStore::open(directory.path()).unwrap();
            let token = store.create_token().unwrap();
            let engine = engine(
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
            );
            let request = || ConversionRequest::new(InputRef::bytes(b"one".as_slice(), Some("x")));
            block_on(engine.convert_recoverable(request(), &store, &token)).unwrap();
            let metadata = store.inspect(&token).unwrap().unwrap();
            let mut payload: serde_json::Value =
                serde_json::from_slice(&store.test_payload(&token, TaskPhase::Succeeded)).unwrap();
            match mutation {
                "markdown" => payload["value"]["markdown"] = serde_json::json!("forged\n"),
                "asset-id" => {
                    let asset = Asset {
                        id: AssetId("duplicate".into()),
                        filename: None,
                        media_type: "image/png".into(),
                        bytes: vec![1],
                        external_uri: None,
                    };
                    payload["value"]["assets"] = serde_json::json!([
                        checkpoint_asset_json(&asset),
                        checkpoint_asset_json(&asset)
                    ]);
                }
                "asset-mime" => {
                    let asset = Asset {
                        id: AssetId("forged".into()),
                        filename: None,
                        media_type: "text/html;unsafe=true".into(),
                        bytes: vec![1],
                        external_uri: None,
                    };
                    payload["value"]["assets"] = serde_json::json!([checkpoint_asset_json(&asset)]);
                }
                "asset-uri" => {
                    let asset = Asset {
                        id: AssetId("forged".into()),
                        filename: None,
                        media_type: "image/png".into(),
                        bytes: Vec::new(),
                        external_uri: Some("https://example.com/image.png?secret=x".into()),
                    };
                    payload["value"]["assets"] = serde_json::json!([checkpoint_asset_json(&asset)]);
                }
                "asset-reference" => {
                    let node = BlockNode {
                        id: into_markdown_core::NodeId("image".into()),
                        block: Block::Image {
                            asset: AssetId("missing".into()),
                            alt: Some("diagram".into()),
                        },
                        provenance: Provenance {
                            kind: ProvenanceKind::NativeParser,
                            provider: "test".into(),
                            locator: SourceLocator::default(),
                            confidence: Some(1.0),
                        },
                    };
                    payload["value"]["document"]["blocks"] = serde_json::json!([node]);
                }
                "diagnostic" => {
                    let diagnostic = Diagnostic {
                        code: "forged".into(),
                        severity: into_markdown_core::DiagnosticSeverity::Warning,
                        message: "forged".into(),
                        locator: Some(SourceLocator { page: Some(0), ..SourceLocator::default() }),
                    };
                    payload["value"]["diagnostics"] = serde_json::json!([diagnostic]);
                }
                "provenance" => {
                    let provenance = Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "forged".into(),
                        locator: SourceLocator::default(),
                        confidence: Some(1.0),
                    };
                    payload["value"]["provenance"] = serde_json::json!([provenance]);
                }
                _ => unreachable!(),
            }
            let payload = serde_json::to_vec(&payload).unwrap();
            store.test_replace_envelope(
                &token,
                TaskPhase::Succeeded,
                1,
                &metadata.input_fingerprint,
                &metadata.options_fingerprint,
                &payload,
            );
            let error =
                block_on(engine.convert_recoverable(request(), &store, &token)).unwrap_err();
            assert_eq!(error.code(), into_markdown_core::ErrorCode::Recovery, "{mutation}");
            assert!(error.to_string().contains("corrupt"), "{mutation}: {error}");
        }
    }
}
