//! Deterministic portable bundle serialization.

use crate::error::{CliError, ExitClass};
use into_markdown::{
    BUNDLE_SCHEMA_VERSION, ConversionOptions, ConversionResult, DTO_SCHEMA_VERSION, DiagnosticsDto,
    ProvenanceListDto, plan_assets,
};
use serde::Serialize;
use std::io::{Seek, Write};
use zip::write::SimpleFileOptions;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamingBundleAsset<'a> {
    id: &'a str,
    source_asset_ids: &'a [String],
    path: String,
    media_type: &'a str,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamingBundleManifest<'a> {
    schema_version: u32,
    markdown: &'static str,
    document_ir: &'static str,
    diagnostics: &'static str,
    diagnostics_schema_version: u32,
    provenance: &'static str,
    provenance_schema_version: u32,
    assets: &'a [StreamingBundleAsset<'a>],
}

/// Stream a deterministic portable bundle to a seekable destination.
pub(crate) fn write_bundle<W: Write + Seek>(
    result: &ConversionResult,
    destination: W,
) -> Result<(), CliError> {
    if let Some(asset) = result.assets.iter().find(|asset| asset.bytes.is_empty()) {
        return Err(CliError::new(
            ExitClass::Conversion,
            "bundleAssetMissingContent",
            format!("bundle asset {} has no portable content", asset.id.0),
        ));
    }
    let mut options = ConversionOptions::default();
    options.output.asset_uri_prefix = Some("assets".into());
    let plan = plan_assets(&result.document, &result.assets, &options).map_err(CliError::from)?;
    let mut assets = Vec::new();
    assets
        .try_reserve_exact(plan.entries().len())
        .map_err(|_| CliError::internal("allocate bounded bundle asset plan"))?;
    for entry in plan.entries() {
        let id = entry
            .asset_ids
            .first()
            .ok_or_else(|| CliError::internal("bundle asset plan omitted its source ID"))?;
        assets.push(StreamingBundleAsset {
            id,
            source_asset_ids: &entry.asset_ids,
            path: format!("assets/{}", entry.filename),
            media_type: &entry.media_type,
            size: entry.size,
        });
    }
    let manifest = StreamingBundleManifest {
        schema_version: BUNDLE_SCHEMA_VERSION,
        markdown: "document.md",
        document_ir: "document.ir.json",
        diagnostics: "diagnostics.json",
        diagnostics_schema_version: DTO_SCHEMA_VERSION,
        provenance: "provenance.json",
        provenance_schema_version: DTO_SCHEMA_VERSION,
        assets: &assets,
    };
    let mut archive = zip::ZipWriter::new(destination);
    let file_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let directory_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    archive
        .start_file("diagnostics.json", file_options)
        .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
    DiagnosticsDto::write_bundle_json_from_diagnostics(&result.diagnostics, &mut archive)
        .map_err(|error| CliError::internal(format!("serialize diagnostics DTO: {error}")))?;
    archive.write_all(b"\n")?;
    result
        .document
        .validate()
        .map_err(|error| CliError::internal(format!("validate document IR: {error}")))?;
    archive
        .start_file("document.ir.json", file_options)
        .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
    serde_json::to_writer_pretty(&mut archive, &result.document)
        .map_err(|error| CliError::internal(format!("serialize document IR: {error}")))?;
    archive.write_all(b"\n")?;
    archive
        .start_file("document.md", file_options)
        .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
    archive.write_all(result.markdown.as_bytes())?;
    archive
        .start_file("manifest.json", file_options)
        .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
    serde_json::to_writer_pretty(&mut archive, &manifest)
        .map_err(|error| CliError::internal(format!("serialize bundle manifest: {error}")))?;
    archive.write_all(b"\n")?;
    archive
        .start_file("provenance.json", file_options)
        .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
    ProvenanceListDto::write_bundle_json_from_provenance(&result.provenance, &mut archive)
        .map_err(|error| CliError::internal(format!("serialize provenance DTO: {error}")))?;
    archive.write_all(b"\n")?;
    archive
        .add_directory("assets/", directory_options)
        .map_err(|error| CliError::internal(format!("create bundle assets directory: {error}")))?;
    for entry in plan.entries() {
        archive
            .start_file(format!("assets/{}", entry.filename), file_options)
            .map_err(|error| CliError::internal(format!("create bundle asset: {error}")))?;
        archive.write_all(&result.assets[entry.source_index].bytes)?;
    }
    archive.finish().map_err(|error| CliError::internal(format!("finish bundle: {error}")))?;
    Ok(())
}
