//! File-backed structured artifact staging.
//!
//! The engine adapter is intentionally added only after the stacked #272
//! branch is approved. Keeping this module independent lets #273 add the
//! smallest sink contract and prepare/execute seam against that stable base.

use crate::args::EmitKind;
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
mod sink;

use io::{ChunkRecordingWriter, IndentingWriter, copy_reader, copy_spool, replay_spool_chunks};
use json::JsonStringSpool;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DocumentPhase {
    AwaitMetadata,
    Blocks,
    Finished,
}

pub(crate) struct StructuredSpool {
    context: ExecutionContext,
    capabilities: ArtifactSinkCapabilities,
    ir: TemporaryFile,
    ir_write_chunks: Vec<usize>,
    _ir_chunk_lease: ResourceReservation,
    markdown: TemporaryFile,
    markdown_json: JsonStringSpool,
    diagnostics: TemporaryFile,
    provenance: TemporaryFile,
    asset_records: Vec<AssetRecord>,
    payload_records: Vec<PayloadRecord>,
    payload_by_filename: HashMap<String, usize>,
    active_asset: Option<ActiveAsset>,
    index_leases: Vec<ResourceReservation>,
    finished: bool,
    serialization_calls: u64,
    document_phase: DocumentPhase,
    first_block: bool,
}

impl StructuredSpool {
    pub(crate) fn new(context: ExecutionContext, emit: EmitKind) -> Result<Self, CliError> {
        let ir = context.temporary_file("into-md-ir").map_err(CliError::from)?;
        let diagnostics = context.temporary_file("into-md-diagnostics").map_err(CliError::from)?;
        let provenance = context.temporary_file("into-md-provenance").map_err(CliError::from)?;
        let markdown = context.temporary_file("into-md-markdown").map_err(CliError::from)?;
        let markdown_json = JsonStringSpool::new(&context, "into-md-markdown-json")?;
        let index_lease = context.reserve_memory(INDEX_BASE_BYTES).map_err(CliError::from)?;
        let ir_chunk_lease = context.reserve_memory(0).map_err(CliError::from)?;
        Ok(Self {
            context,
            capabilities: ArtifactSinkCapabilities {
                markdown: emit != EmitKind::IrJson,
                semantic_events: true,
                assets: true,
            },
            ir,
            ir_write_chunks: Vec::new(),
            _ir_chunk_lease: ir_chunk_lease,
            markdown,
            markdown_json,
            diagnostics,
            provenance,
            asset_records: Vec::new(),
            payload_records: Vec::new(),
            payload_by_filename: HashMap::new(),
            active_asset: None,
            index_leases: vec![index_lease],
            finished: false,
            serialization_calls: 0,
            document_phase: DocumentPhase::AwaitMetadata,
            first_block: true,
        })
    }

    #[cfg(test)]
    pub(super) fn from_result(
        result: &ConversionResult,
        context: ExecutionContext,
    ) -> Result<Self, CliError> {
        let mut spool = Self::new(context, EmitKind::ResultJson)?;
        spool
            .write_document_event(&DocumentStreamEvent::Metadata(&result.document.metadata))
            .map_err(CliError::from)?;
        for block in &result.document.blocks {
            spool
                .write_document_event(&DocumentStreamEvent::RootBlock(block))
                .map_err(CliError::from)?;
        }
        spool.finish_document(&result.diagnostics, &result.provenance).map_err(CliError::from)?;
        spool.write_markdown(result.markdown.as_bytes())?;
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some("assets".into());
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
        spool.finish()?;
        Ok(spool)
    }

    pub(crate) fn finish(&mut self) -> Result<(), CliError> {
        if self.finished {
            return Ok(());
        }
        if self.active_asset.is_some() {
            return Err(CliError::internal("conversion ended during an asset stream"));
        }
        if self.document_phase != DocumentPhase::Finished {
            return Err(CliError::internal("semantic document was not finalized"));
        }
        self.markdown_json.finish()?;
        self.ir.sync_all().map_err(CliError::from)?;
        self.markdown.sync_all().map_err(CliError::from)?;
        self.diagnostics.sync_all().map_err(CliError::from)?;
        self.provenance.sync_all().map_err(CliError::from)?;
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
        self.serialization_calls = self.serialization_calls.saturating_add(1);
        match emit {
            EmitKind::Markdown => copy_spool(&self.context, &self.markdown, destination),
            EmitKind::IrJson => copy_spool(&self.context, &self.ir, destination),
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
}

mod result;

use result::map_json_error;

#[cfg(test)]
#[path = "stream/tests.rs"]
mod tests;
