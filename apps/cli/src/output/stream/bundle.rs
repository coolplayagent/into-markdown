use super::*;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleAssetWire<'a> {
    id: &'a str,
    source_asset_ids: &'a [String],
    path: String,
    media_type: &'a str,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleManifestWire<'a> {
    schema_version: u32,
    markdown: &'static str,
    document_ir: &'static str,
    diagnostics: &'static str,
    diagnostics_schema_version: u32,
    provenance: &'static str,
    provenance_schema_version: u32,
    assets: &'a [BundleAssetWire<'a>],
}

struct BundleAssetGroup {
    payload_index: usize,
    ids: Vec<String>,
}

impl StructuredSpool {
    fn bundle_groups(&self) -> Result<Vec<BundleAssetGroup>, CliError> {
        let mut groups = self
            .payload_records
            .iter()
            .enumerate()
            .map(|(payload_index, _)| BundleAssetGroup { payload_index, ids: Vec::new() })
            .collect::<Vec<_>>();
        for asset in &self.asset_records {
            let payload_index = asset.payload_index.ok_or_else(|| {
                CliError::new(
                    ExitClass::Conversion,
                    "bundleAssetMissingContent",
                    format!("bundle asset {} has no portable content", asset.id),
                )
            })?;
            groups
                .get_mut(payload_index)
                .ok_or_else(|| CliError::internal("bundle payload index is inconsistent"))?
                .ids
                .push(asset.id.clone());
        }
        for group in &mut groups {
            group.ids.sort();
        }
        groups.sort_by(|left, right| {
            self.payload_records[left.payload_index]
                .filename
                .cmp(&self.payload_records[right.payload_index].filename)
        });
        Ok(groups)
    }

    pub(super) fn write_bundle<W: Write + Seek>(
        &self,
        destination: &mut W,
    ) -> Result<(), CliError> {
        let groups = self.bundle_groups()?;
        let manifest_assets = groups
            .iter()
            .map(|group| {
                let payload = &self.payload_records[group.payload_index];
                let id = group
                    .ids
                    .first()
                    .ok_or_else(|| CliError::internal("bundle asset group is empty"))?;
                Ok(BundleAssetWire {
                    id,
                    source_asset_ids: &group.ids,
                    path: format!("assets/{}", payload.filename),
                    media_type: &payload.media_type,
                    size: payload.size,
                })
            })
            .collect::<Result<Vec<_>, CliError>>()?;
        let manifest = BundleManifestWire {
            schema_version: BUNDLE_SCHEMA_VERSION,
            markdown: "document.md",
            document_ir: "document.ir.json",
            diagnostics: "diagnostics.json",
            diagnostics_schema_version: DTO_SCHEMA_VERSION,
            provenance: "provenance.json",
            provenance_schema_version: DTO_SCHEMA_VERSION,
            assets: &manifest_assets,
        };
        let mut archive = zip::ZipWriter::new(destination);
        let files = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        let directory = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        archive
            .start_file("diagnostics.json", files)
            .map_err(|error| map_zip_error(error, "create bundle entry"))?;
        copy_spool(&self.context, &self.diagnostics, &mut archive)?;
        archive
            .start_file("document.ir.json", files)
            .map_err(|error| map_zip_error(error, "create bundle entry"))?;
        replay_spool_chunks(&self.context, &self.ir, &self.ir_write_chunks, &mut archive)?;
        archive
            .start_file("document.md", files)
            .map_err(|error| map_zip_error(error, "create bundle entry"))?;
        copy_spool(&self.context, &self.markdown, &mut archive)?;
        archive
            .start_file("manifest.json", files)
            .map_err(|error| map_zip_error(error, "create bundle entry"))?;
        serde_json::to_writer_pretty(&mut archive, &manifest)
            .map_err(|error| map_json_error(&error, "serialize bundle manifest"))?;
        archive.write_all(b"\n")?;
        archive
            .start_file("provenance.json", files)
            .map_err(|error| map_zip_error(error, "create bundle entry"))?;
        copy_spool(&self.context, &self.provenance, &mut archive)?;
        archive
            .add_directory("assets/", directory)
            .map_err(|error| map_zip_error(error, "create bundle assets directory"))?;
        for group in groups {
            let payload = &self.payload_records[group.payload_index];
            archive
                .start_file(format!("assets/{}", payload.filename), files)
                .map_err(|error| map_zip_error(error, "create bundle asset"))?;
            let mut reader = payload.file.as_file().map_err(CliError::from)?.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            copy_reader(&self.context, &mut reader.take(payload.size), &mut archive)?;
        }
        archive.finish().map_err(|error| map_zip_error(error, "finish bundle"))?;
        Ok(())
    }
}

fn map_zip_error(error: zip::result::ZipError, operation: &str) -> CliError {
    match error {
        zip::result::ZipError::Io(error) => CliError::from(error),
        error => CliError::internal(format!("{operation}: {error}")),
    }
}
