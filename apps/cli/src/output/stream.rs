//! File-backed structured artifact staging.
//!
//! The immutable plan selects only the representations required by the
//! requested wire artifact and asset mode.

use crate::args::{AssetModeArg, EmitKind};
use crate::error::{CliError, ExitClass};
use base64::engine::general_purpose::STANDARD;
use into_markdown::{
    ArtifactSink, ArtifactSinkCapabilities, AssetStreamInfo, BUNDLE_SCHEMA_VERSION,
    ConversionError, DTO_SCHEMA_VERSION, DiagnosticsDto, DocumentStreamEvent, ExecutionContext,
    ProvenanceListDto, ResourceReservation, TemporaryFile, asset_filename_from_sha256,
};
#[cfg(test)]
use into_markdown::{ConversionOptions, ConversionResult, plan_assets};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use zip::write::SimpleFileOptions;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const INDEX_BASE_BYTES: u64 = 4 * 1024;
const INDEX_ASSET_BYTES: u64 = 4 * 1024;

#[derive(Clone)]
struct AssetRecord {
    id: String,
    wire_filename: Option<String>,
    storage_filename: Option<String>,
    media_type: String,
    external_uri: Option<String>,
    payload_index: Option<usize>,
}

struct PayloadRecord {
    filename: String,
    media_type: String,
    file: TemporaryFile,
    size: u64,
    sha256: [u8; 32],
}

struct ActiveAsset {
    record: AssetRecord,
    expected: u64,
    written: u64,
    existing_payload: Option<usize>,
    file: Option<TemporaryFile>,
    hash: Sha256,
    announced_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Copy)]
struct AssetStart<'a> {
    id: &'a str,
    wire_filename: Option<&'a str>,
    storage_filename: Option<&'a str>,
    media_type: &'a str,
    external_uri: Option<&'a str>,
    size: u64,
    content_sha256: Option<[u8; 32]>,
}

mod bundle;
mod document;
mod io;
mod json;
mod plan;
mod sink;

use io::{ChunkRecordingWriter, IndentingWriter, copy_reader, copy_spool, replay_spool_chunks};
use json::JsonStringSpool;
use plan::RepresentationPlan;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DocumentPhase {
    AwaitMetadata,
    Blocks,
    Finished,
}

pub(crate) struct StructuredSpool {
    context: ExecutionContext,
    plan: RepresentationPlan,
    capabilities: ArtifactSinkCapabilities,
    ir: Option<TemporaryFile>,
    ir_write_chunks: Vec<usize>,
    ir_chunk_lease: Option<ResourceReservation>,
    markdown: Option<TemporaryFile>,
    markdown_json: Option<JsonStringSpool>,
    diagnostics: Option<TemporaryFile>,
    provenance: Option<TemporaryFile>,
    asset_records: Vec<AssetRecord>,
    payload_records: Vec<PayloadRecord>,
    payload_by_filename: HashMap<String, usize>,
    active_asset: Option<ActiveAsset>,
    index_leases: Vec<ResourceReservation>,
    finished: bool,
    serialization_calls: u64,
    document_phase: Option<DocumentPhase>,
    first_block: bool,
}

impl StructuredSpool {
    pub(crate) fn new(
        context: ExecutionContext,
        emit: EmitKind,
        asset_mode: AssetModeArg,
    ) -> Result<Self, CliError> {
        let plan = RepresentationPlan::new(emit, asset_mode);
        let ir = plan
            .semantic_ir()
            .then(|| context.temporary_file("into-md-ir").map_err(CliError::from))
            .transpose()?;
        let diagnostics = plan
            .inventories()
            .then(|| context.temporary_file("into-md-diagnostics").map_err(CliError::from))
            .transpose()?;
        let provenance = plan
            .inventories()
            .then(|| context.temporary_file("into-md-provenance").map_err(CliError::from))
            .transpose()?;
        let markdown = plan
            .raw_markdown()
            .then(|| context.temporary_file("into-md-markdown").map_err(CliError::from))
            .transpose()?;
        let markdown_json = plan
            .escaped_markdown()
            .then(|| JsonStringSpool::new(&context, "into-md-markdown-json"))
            .transpose()?;
        let index_leases = if plan.semantic_ir() || plan.assets {
            vec![context.reserve_memory(INDEX_BASE_BYTES).map_err(CliError::from)?]
        } else {
            Vec::new()
        };
        let ir_chunk_lease = plan
            .semantic_ir()
            .then(|| context.reserve_memory(0).map_err(CliError::from))
            .transpose()?;
        Ok(Self {
            context,
            plan,
            capabilities: plan.capabilities(),
            ir,
            ir_write_chunks: Vec::new(),
            ir_chunk_lease,
            markdown,
            markdown_json,
            diagnostics,
            provenance,
            asset_records: Vec::new(),
            payload_records: Vec::new(),
            payload_by_filename: HashMap::new(),
            active_asset: None,
            index_leases,
            finished: false,
            serialization_calls: 0,
            document_phase: plan.semantic_ir().then_some(DocumentPhase::AwaitMetadata),
            first_block: true,
        })
    }

    #[cfg(test)]
    pub(super) fn from_result(
        result: &ConversionResult,
        context: ExecutionContext,
        emit: EmitKind,
        asset_mode: AssetModeArg,
    ) -> Result<Self, CliError> {
        let mut spool = Self::new(context, emit, asset_mode)?;
        if spool.capabilities.semantic_events {
            spool
                .write_document_event(&DocumentStreamEvent::Metadata(&result.document.metadata))
                .map_err(CliError::from)?;
            for block in &result.document.blocks {
                spool
                    .write_document_event(&DocumentStreamEvent::RootBlock(block))
                    .map_err(CliError::from)?;
            }
            spool
                .finish_document(&result.diagnostics, &result.provenance)
                .map_err(CliError::from)?;
        }
        if spool.capabilities.markdown {
            spool.write_markdown(result.markdown.as_bytes())?;
        }
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some("assets".into());
        if spool.capabilities.assets {
            let plan =
                plan_assets(&result.document, &result.assets, &options).map_err(CliError::from)?;
            let storage_by_id = plan
                .entries()
                .iter()
                .flat_map(|entry| {
                    entry.asset_ids.iter().map(|id| (id.as_str(), entry.filename.as_str()))
                })
                .collect::<HashMap<_, _>>();
            for asset in &result.assets {
                let storage_filename = storage_by_id.get(asset.id.0.as_str()).copied();
                spool.begin_asset_inner(AssetStart {
                    id: &asset.id.0,
                    wire_filename: asset.filename.as_deref(),
                    storage_filename,
                    media_type: &asset.media_type,
                    external_uri: asset.external_uri.as_deref(),
                    size: u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX),
                    content_sha256: (!asset.bytes.is_empty())
                        .then(|| Sha256::digest(&asset.bytes).into()),
                })?;
                for chunk in asset.bytes.chunks(COPY_BUFFER_BYTES) {
                    spool.write_asset_inner(chunk)?;
                }
                spool.end_asset_inner()?;
            }
        }
        spool.finish()?;
        Ok(spool)
    }

    pub(crate) fn finish(&mut self) -> Result<(), CliError> {
        if self.finished {
            return Ok(());
        }
        self.validate_representations()?;
        if self.active_asset.is_some() {
            return Err(CliError::internal("conversion ended during an asset stream"));
        }
        if self.document_phase.is_some_and(|phase| phase != DocumentPhase::Finished) {
            return Err(CliError::internal("semantic document was not finalized"));
        }
        if let Some(markdown_json) = self.markdown_json.as_mut() {
            markdown_json.finish()?;
        }
        for spool in [
            self.ir.as_mut(),
            self.markdown.as_mut(),
            self.diagnostics.as_mut(),
            self.provenance.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            spool.sync_all().map_err(CliError::from)?;
        }
        for payload in &mut self.payload_records {
            payload.file.sync_all().map_err(CliError::from)?;
        }
        self.finished = true;
        Ok(())
    }

    pub(crate) fn serialize<W: Write + Seek>(
        &mut self,
        emit: EmitKind,
        destination: &mut W,
    ) -> Result<(), CliError> {
        if !self.finished {
            return Err(CliError::internal("structured spool is not finalized"));
        }
        if emit != self.plan.emit {
            return Err(CliError::internal(
                "requested emit does not match the frozen representation plan",
            ));
        }
        self.validate_representations()?;
        self.serialization_calls = self.serialization_calls.saturating_add(1);
        match emit {
            EmitKind::Markdown => copy_spool(
                &self.context,
                self.markdown
                    .as_ref()
                    .ok_or_else(|| CliError::internal("raw Markdown representation is absent"))?,
                destination,
            ),
            EmitKind::IrJson => copy_spool(
                &self.context,
                self.ir
                    .as_ref()
                    .ok_or_else(|| CliError::internal("document IR representation is absent"))?,
                destination,
            ),
            EmitKind::ResultJson => self.write_result_json(destination),
            EmitKind::Bundle => self.write_bundle(destination),
        }
    }

    pub(crate) fn external_payloads(&self) -> Result<Vec<(&str, &std::fs::File)>, CliError> {
        self.payload_records
            .iter()
            .map(|payload| {
                payload
                    .file
                    .as_file()
                    .map(|file| (payload.filename.as_str(), file))
                    .map_err(CliError::from)
            })
            .collect()
    }

    pub(crate) fn has_payloads(&self) -> bool {
        !self.payload_records.is_empty()
    }

    fn validate_representations(&self) -> Result<(), CliError> {
        let valid = self.markdown.is_some() == self.plan.raw_markdown()
            && self.markdown_json.is_some() == self.plan.escaped_markdown()
            && self.ir.is_some() == self.plan.semantic_ir()
            && self.ir_chunk_lease.is_some() == self.plan.semantic_ir()
            && self.document_phase.is_some() == self.plan.semantic_ir()
            && self.diagnostics.is_some() == self.plan.inventories()
            && self.provenance.is_some() == self.plan.inventories()
            && (self.plan.assets
                || (self.asset_records.is_empty()
                    && self.payload_records.is_empty()
                    && self.active_asset.is_none()));
        if valid {
            Ok(())
        } else {
            Err(CliError::internal(
                "structured spool does not match its frozen representation plan",
            ))
        }
    }
}

mod result;

use result::map_json_error;

#[cfg(test)]
#[path = "stream/tests.rs"]
mod tests;
