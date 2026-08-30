//! Engine sink callbacks and content-addressed asset staging.

use super::*;

impl StructuredSpool {
    pub(super) fn write_markdown(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        self.markdown.write_all_checked(chunk).map_err(CliError::from)?;
        self.markdown_json.write(chunk)
    }

    pub(super) fn begin_asset_inner(&mut self, asset: AssetStart<'_>) -> Result<(), CliError> {
        if self.active_asset.is_some() {
            return Err(CliError::internal("nested asset stream"));
        }
        let lease =
            self.context.reserve_memory(asset_index_bytes(&asset)?).map_err(CliError::from)?;
        self.asset_records
            .try_reserve(1)
            .map_err(|error| CliError::internal(format!("reserve asset index: {error}")))?;
        self.index_leases.push(lease);
        let storage_filename = match (asset.storage_filename, asset.content_sha256) {
            (Some(filename), _) => Some(filename.to_owned()),
            (None, Some(digest)) => Some(storage_filename(digest, asset.media_type)?),
            (None, None) => None,
        };
        let existing_payload = storage_filename
            .as_deref()
            .and_then(|filename| self.payload_by_filename.get(filename).copied());
        let file = if existing_payload.is_none() && storage_filename.is_some() {
            Some(self.context.temporary_file("into-md-asset").map_err(CliError::from)?)
        } else {
            None
        };
        self.active_asset = Some(ActiveAsset {
            record: AssetRecord {
                id: asset.id.to_owned(),
                wire_filename: asset.wire_filename.map(str::to_owned),
                storage_filename,
                media_type: asset.media_type.to_owned(),
                external_uri: asset.external_uri.map(str::to_owned),
                payload_index: existing_payload,
            },
            expected: asset.size,
            written: 0,
            existing_payload,
            file,
            hash: Sha256::new(),
            announced_sha256: asset.content_sha256,
        });
        Ok(())
    }

    pub(super) fn write_asset_inner(&mut self, chunk: &[u8]) -> Result<(), CliError> {
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
        if let Some(file) = active.file.as_mut() {
            file.write_all_checked(chunk).map_err(CliError::from)?;
        }
        Ok(())
    }

    pub(super) fn end_asset_inner(&mut self) -> Result<(), CliError> {
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
        if active.announced_sha256.is_some_and(|announced| announced != digest) {
            return Err(CliError::new(
                ExitClass::Conversion,
                "assetDigestMismatch",
                "asset payload did not match its announced SHA-256",
            ));
        }
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
                file: active.file.take().ok_or_else(|| {
                    CliError::internal("unique asset payload has no backing temporary file")
                })?,
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
}

impl ArtifactSink for StructuredSpool {
    fn capabilities(&self) -> ArtifactSinkCapabilities {
        self.capabilities
    }

    fn write_markdown(&mut self, chunk: &[u8]) -> Result<(), ConversionError> {
        StructuredSpool::write_markdown(self, chunk).map_err(conversion_from_cli)
    }

    fn begin_asset(&mut self, asset: &AssetStreamInfo) -> Result<(), ConversionError> {
        self.begin_asset_inner(AssetStart {
            id: &asset.id.0,
            wire_filename: asset.filename.as_deref(),
            storage_filename: None,
            media_type: &asset.media_type,
            external_uri: asset.external_uri.as_deref(),
            size: asset.size,
            content_sha256: asset.content_sha256,
        })
        .map_err(conversion_from_cli)
    }

    fn write_asset(&mut self, chunk: &[u8]) -> Result<(), ConversionError> {
        self.write_asset_inner(chunk).map_err(conversion_from_cli)
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        self.end_asset_inner().map_err(conversion_from_cli)
    }

    fn write_document_event(
        &mut self,
        event: &DocumentStreamEvent<'_>,
    ) -> Result<(), ConversionError> {
        self.write_document_event_inner(event).map_err(conversion_from_cli)
    }

    fn finish_document(
        &mut self,
        diagnostics: &[into_markdown::Diagnostic],
        provenance: &[into_markdown::Provenance],
    ) -> Result<(), ConversionError> {
        self.finish_document_inner(diagnostics, provenance).map_err(conversion_from_cli)
    }
}

fn conversion_from_cli(error: CliError) -> ConversionError {
    match error.code() {
        "cancelled" => ConversionError::Cancelled,
        "resourceLimit" => {
            let (limit, detail) = error.limit().unwrap_or(("max_memory_bytes", error.message()));
            let limit = match limit {
                "max_memory_bytes" => "max_memory_bytes",
                "max_temporary_bytes" => "max_temporary_bytes",
                "max_output_bytes" => "max_output_bytes",
                "max_asset_bytes" => "max_asset_bytes",
                "max_total_asset_bytes" => "max_total_asset_bytes",
                _ => "max_memory_bytes",
            };
            ConversionError::ResourceLimit { limit, detail: detail.to_owned() }
        }
        "io" | "brokenPipe" => ConversionError::Io { detail: error.message().to_owned() },
        _ => ConversionError::Internal { detail: error.to_string() },
    }
}

fn storage_filename(digest: [u8; 32], media_type: &str) -> Result<String, CliError> {
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}")
            .map_err(|error| CliError::internal(format!("format asset digest: {error}")))?;
    }
    asset_filename_from_sha256(&hex, media_type).map_err(CliError::from)
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
