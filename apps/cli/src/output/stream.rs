//! File-backed structured artifact staging.
//!
//! The engine adapter is intentionally added only after the stacked #272
//! branch is approved. Keeping this module independent lets #273 add the
//! smallest sink contract and prepare/execute seam against that stable base.

use crate::args::EmitKind;
use crate::error::{CliError, ExitClass};
use base64::engine::general_purpose::STANDARD;
use into_markdown::{
    BUNDLE_SCHEMA_VERSION, ConversionError, ConversionOptions, ConversionResult,
    DTO_SCHEMA_VERSION, DiagnosticsDto, Document, ExecutionContext, ProvenanceListDto,
    ResourceReservation, TemporaryFile, plan_assets,
};
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
    offset: u64,
    size: u64,
    sha256: [u8; 32],
}

struct ActiveAsset {
    record: AssetRecord,
    expected: u64,
    written: u64,
    existing_payload: Option<usize>,
    offset: u64,
    hash: Sha256,
}

#[derive(Clone, Copy)]
struct AssetStart<'a> {
    id: &'a str,
    wire_filename: Option<&'a str>,
    storage_filename: Option<&'a str>,
    media_type: &'a str,
    external_uri: Option<&'a str>,
    size: u64,
}

mod bundle;
mod json;

use json::JsonStringSpool;

struct StructuredSpool {
    context: ExecutionContext,
    ir: TemporaryFile,
    ir_write_chunks: Vec<usize>,
    _ir_chunk_lease: ResourceReservation,
    markdown: TemporaryFile,
    markdown_json: JsonStringSpool,
    diagnostics: TemporaryFile,
    provenance: TemporaryFile,
    payloads: TemporaryFile,
    asset_records: Vec<AssetRecord>,
    payload_records: Vec<PayloadRecord>,
    payload_by_filename: HashMap<String, usize>,
    active_asset: Option<ActiveAsset>,
    index_leases: Vec<ResourceReservation>,
    finished: bool,
    serialization_calls: u64,
}

impl StructuredSpool {
    fn from_result(result: &ConversionResult, context: ExecutionContext) -> Result<Self, CliError> {
        let mut ir = context.temporary_file("into-md-ir").map_err(CliError::from)?;
        let mut ir_write_chunks = Vec::new();
        let mut ir_chunk_lease = context.reserve_memory(0).map_err(CliError::from)?;
        result
            .document
            .validate()
            .map_err(|error| CliError::internal(format!("validate document IR: {error}")))?;
        serde_json::to_writer_pretty(
            &mut ChunkRecordingWriter {
                destination: &mut ir,
                chunks: &mut ir_write_chunks,
                lease: &mut ir_chunk_lease,
            },
            &result.document,
        )
        .map_err(|error| map_json_error(&error, "serialize document IR"))?;
        ir.write_all_checked(b"\n").map_err(CliError::from)?;

        let mut diagnostics =
            context.temporary_file("into-md-diagnostics").map_err(CliError::from)?;
        DiagnosticsDto::write_bundle_json_from_diagnostics(&result.diagnostics, &mut diagnostics)
            .map_err(|error| CliError::internal(format!("serialize diagnostics DTO: {error}")))?;
        diagnostics.write_all_checked(b"\n").map_err(CliError::from)?;

        let mut provenance =
            context.temporary_file("into-md-provenance").map_err(CliError::from)?;
        ProvenanceListDto::write_bundle_json_from_provenance(&result.provenance, &mut provenance)
            .map_err(|error| CliError::internal(format!("serialize provenance DTO: {error}")))?;
        provenance.write_all_checked(b"\n").map_err(CliError::from)?;

        let markdown = context.temporary_file("into-md-markdown").map_err(CliError::from)?;
        let markdown_json = JsonStringSpool::new(&context, "into-md-markdown-json")?;
        let payloads = context.temporary_file("into-md-assets").map_err(CliError::from)?;
        let index_lease = context.reserve_memory(INDEX_BASE_BYTES).map_err(CliError::from)?;
        let mut spool = Self {
            context,
            ir,
            ir_write_chunks,
            _ir_chunk_lease: ir_chunk_lease,
            markdown,
            markdown_json,
            diagnostics,
            provenance,
            payloads,
            asset_records: Vec::new(),
            payload_records: Vec::new(),
            payload_by_filename: HashMap::new(),
            active_asset: None,
            index_leases: vec![index_lease],
            finished: false,
            serialization_calls: 0,
        };
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
            spool.begin_asset(AssetStart {
                id: &asset.id.0,
                wire_filename: asset.filename.as_deref(),
                storage_filename,
                media_type: &asset.media_type,
                external_uri: asset.external_uri.as_deref(),
                size: u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX),
            })?;
            for chunk in asset.bytes.chunks(COPY_BUFFER_BYTES) {
                spool.write_asset(chunk)?;
            }
            spool.end_asset()?;
        }
        spool.finish()?;
        Ok(spool)
    }

    fn write_markdown(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        self.markdown.write_all_checked(chunk).map_err(CliError::from)?;
        self.markdown_json.write(chunk)
    }

    fn begin_asset(&mut self, asset: AssetStart<'_>) -> Result<(), CliError> {
        if self.active_asset.is_some() {
            return Err(CliError::internal("nested asset stream"));
        }
        let lease =
            self.context.reserve_memory(asset_index_bytes(&asset)?).map_err(CliError::from)?;
        self.asset_records
            .try_reserve(1)
            .map_err(|error| CliError::internal(format!("reserve asset index: {error}")))?;
        self.index_leases.push(lease);
        let existing_payload = asset
            .storage_filename
            .and_then(|filename| self.payload_by_filename.get(filename).copied());
        let offset = self.payloads.seek(SeekFrom::End(0))?;
        self.active_asset = Some(ActiveAsset {
            record: AssetRecord {
                id: asset.id.to_owned(),
                wire_filename: asset.wire_filename.map(str::to_owned),
                storage_filename: asset.storage_filename.map(str::to_owned),
                media_type: asset.media_type.to_owned(),
                external_uri: asset.external_uri.map(str::to_owned),
                payload_index: existing_payload,
            },
            expected: asset.size,
            written: 0,
            existing_payload,
            offset,
            hash: Sha256::new(),
        });
        Ok(())
    }

    fn write_asset(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        let active = self
            .active_asset
            .as_mut()
            .ok_or_else(|| CliError::internal("asset bytes arrived outside a stream"))?;
        active.written = active
            .written
            .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| CliError::internal("asset byte count overflow"))?;
        if active.written > active.expected {
            return Err(CliError::internal("asset stream exceeded its announced size"));
        }
        active.hash.update(chunk);
        if active.existing_payload.is_none() {
            self.payloads.write_all_checked(chunk).map_err(CliError::from)?;
        }
        Ok(())
    }

    fn end_asset(&mut self) -> Result<(), CliError> {
        let mut active = self
            .active_asset
            .take()
            .ok_or_else(|| CliError::internal("asset stream ended without a beginning"))?;
        if active.written != active.expected {
            return Err(CliError::internal(format!(
                "asset stream ended at {} of {} bytes",
                active.written, active.expected
            )));
        }
        let digest: [u8; 32] = active.hash.finalize().into();
        if let Some(index) = active.existing_payload {
            let existing = self
                .payload_records
                .get(index)
                .ok_or_else(|| CliError::internal("asset payload index is inconsistent"))?;
            if existing.size != active.written
                || existing.sha256 != digest
                || existing.media_type != active.record.media_type
            {
                return Err(CliError::new(
                    ExitClass::Conversion,
                    "assetMetadataConflict",
                    "content-addressed asset metadata did not match its prior payload",
                ));
            }
        } else if let Some(filename) = active.record.storage_filename.as_ref() {
            let index = self.payload_records.len();
            self.payload_records
                .try_reserve(1)
                .map_err(|error| CliError::internal(format!("reserve payload index: {error}")))?;
            self.payload_by_filename
                .try_reserve(1)
                .map_err(|error| CliError::internal(format!("reserve payload lookup: {error}")))?;
            self.payload_records.push(PayloadRecord {
                filename: filename.clone(),
                media_type: active.record.media_type.clone(),
                offset: active.offset,
                size: active.written,
                sha256: digest,
            });
            self.payload_by_filename.insert(filename.clone(), index);
            active.record.payload_index = Some(index);
        } else if active.written != 0 {
            return Err(CliError::internal("asset content has no stable storage filename"));
        }
        self.asset_records.push(active.record);
        Ok(())
    }

    fn finish(&mut self) -> Result<(), CliError> {
        if self.finished {
            return Ok(());
        }
        if self.active_asset.is_some() {
            return Err(CliError::internal("conversion ended during an asset stream"));
        }
        self.markdown_json.finish()?;
        self.finished = true;
        Ok(())
    }

    fn serialize<W: Write + Seek>(
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
}

fn asset_index_bytes(asset: &AssetStart<'_>) -> Result<u64, CliError> {
    let strings = [
        Some(asset.id),
        asset.wire_filename,
        asset.storage_filename,
        Some(asset.media_type),
        asset.external_uri,
    ]
    .into_iter()
    .flatten()
    .try_fold(0_u64, |total, value| {
        total.checked_add(u64::try_from(value.len()).unwrap_or(u64::MAX))
    })
    .ok_or_else(|| {
        CliError::from(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "asset index metadata length overflowed".into(),
        })
    })?;
    strings.checked_mul(4).and_then(|bytes| bytes.checked_add(INDEX_ASSET_BYTES)).ok_or_else(|| {
        CliError::from(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "asset index memory estimate overflowed".into(),
        })
    })
}

fn copy_spool<W: Write>(
    context: &ExecutionContext,
    spool: &TemporaryFile,
    destination: &mut W,
) -> Result<(), CliError> {
    let mut reader = spool.as_file().map_err(CliError::from)?.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    copy_reader(context, &mut reader, destination)
}

struct ChunkRecordingWriter<'a> {
    destination: &'a mut TemporaryFile,
    chunks: &'a mut Vec<usize>,
    lease: &'a mut ResourceReservation,
}

impl Write for ChunkRecordingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.reserve_slot()?;
        match self.destination.write(bytes) {
            Ok(written) => {
                self.chunks.push(written);
                Ok(written)
            }
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(self.destination)
    }
}

impl ChunkRecordingWriter<'_> {
    fn reserve_slot(&mut self) -> std::io::Result<()> {
        if self.chunks.len() < self.chunks.capacity() {
            return Ok(());
        }
        let old_capacity = self.chunks.capacity();
        let target = old_capacity.saturating_mul(2).max(64);
        let slot_bytes = std::mem::size_of::<usize>();
        let planned_bytes = target
            .checked_sub(old_capacity)
            .and_then(|slots| slots.checked_mul(slot_bytes))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
        self.lease.grow(planned_bytes).map_err(std::io::Error::other)?;
        if let Err(error) = self.chunks.try_reserve_exact(target - self.chunks.len()) {
            self.lease.shrink(planned_bytes).map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(error));
        }
        let actual_capacity = self.chunks.capacity();
        if actual_capacity < target {
            *self.chunks = Vec::new();
            let target_bytes = target
                .checked_mul(slot_bytes)
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
            self.lease.shrink(target_bytes).map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(
                "IR replay index reserve returned less than requested capacity",
            ));
        }
        if actual_capacity > target {
            let extra_bytes = actual_capacity
                .checked_sub(target)
                .and_then(|slots| slots.checked_mul(slot_bytes))
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
            if let Err(error) = self.lease.grow(extra_bytes) {
                *self.chunks = Vec::new();
                let target_bytes = target
                    .checked_mul(slot_bytes)
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
                self.lease.shrink(target_bytes).map_err(std::io::Error::other)?;
                return Err(std::io::Error::other(error));
            }
        }
        Ok(())
    }
}

fn replay_spool_chunks<W: Write>(
    context: &ExecutionContext,
    spool: &TemporaryFile,
    chunks: &[usize],
    destination: &mut W,
) -> Result<(), CliError> {
    let maximum = chunks.iter().copied().max().unwrap_or(0);
    let memory = context
        .reserve_memory(u64::try_from(maximum).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(maximum)
        .map_err(|error| CliError::internal(format!("reserve IR replay buffer: {error}")))?;
    buffer.resize(maximum, 0);
    let mut reader = spool.as_file().map_err(CliError::from)?.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    for &length in chunks {
        context.checkpoint().map_err(CliError::from)?;
        reader.read_exact(&mut buffer[..length])?;
        destination.write_all(&buffer[..length])?;
    }
    drop(memory);
    Ok(())
}

fn copy_reader<R: Read, W: Write>(
    context: &ExecutionContext,
    reader: &mut R,
    destination: &mut W,
) -> Result<(), CliError> {
    let _buffer_lease = context
        .reserve_memory(u64::try_from(COPY_BUFFER_BYTES).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    loop {
        context.checkpoint().map_err(CliError::from)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        destination.write_all(&buffer[..read])?;
    }
}

mod result;

use result::map_json_error;

#[cfg(test)]
#[path = "stream/tests.rs"]
mod tests;
