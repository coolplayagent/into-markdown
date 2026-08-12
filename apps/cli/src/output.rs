//! Stable CLI serialization, bundles, batch reports, and atomic output writes.

use crate::args::{AssetModeArg, ConflictPolicy, EmitKind};
use crate::error::{CliError, ExitClass};
use into_markdown::{
    BUNDLE_SCHEMA_VERSION, BatchReportDto, BundleAssetDto, BundleManifestDto, ConversionOptions,
    ConversionResult, DTO_SCHEMA_VERSION, DiagnosticsDto, DtoJsonStyle, ExecutionContext,
    ProvenanceListDto, ResultDto, plan_assets,
};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

pub use into_markdown::{BatchItemDto as BatchItemReport, BatchItemStatus};
pub type BatchReport = BatchReportDto;

/// Serialize a conversion result into the selected primary artifact.
pub fn encode_result(result: &ConversionResult, emit: EmitKind) -> Result<Vec<u8>, CliError> {
    match emit {
        EmitKind::Markdown => Ok(result.markdown.as_bytes().to_vec()),
        EmitKind::IrJson => encode_document(&result.document),
        EmitKind::ResultJson => {
            let mut json = Vec::new();
            ResultDto::write_json_from_result(result, DtoJsonStyle::Pretty, &mut json)
                .map_err(|error| CliError::internal(format!("serialize result DTO: {error}")))?;
            json.push(b'\n');
            Ok(json)
        }
        EmitKind::Bundle => encode_bundle(result),
    }
}

/// Write extracted assets using safe, deterministic filenames.
#[cfg(test)]
pub fn write_assets(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
) -> Result<Vec<WriteOutcome>, CliError> {
    write_assets_with_hook(result, directory, mode, conflict, || Ok(()))
}

/// Fully staged external assets whose targets have not been mutated yet.
pub struct StagedAssets {
    stage: tempfile::TempDir,
    targets: Vec<PathBuf>,
    staged: Vec<PathBuf>,
    conflict: ConflictPolicy,
    context: ExecutionContext,
}

/// Preflight, write, and fsync every external asset without changing targets.
pub fn stage_assets(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<StagedAssets, CliError> {
    let planned = plan_asset_writes(result, directory, mode, conflict)?;
    if planned.is_empty() {
        return Ok(StagedAssets {
            stage: tempfile::tempdir()?,
            targets: vec![],
            staged: vec![],
            conflict,
            context: context.clone(),
        });
    }
    let targets_with_bytes = planned
        .iter()
        .map(|(source_index, path)| (path.clone(), result.assets[*source_index].bytes.as_slice()))
        .collect::<Vec<_>>();
    let root = common_existing_ancestor(&targets_with_bytes)?;
    reject_symlink_components(&root)?;
    ensure_same_filesystem(&root, &targets_with_bytes)?;
    let stage = tempfile::Builder::new().prefix(".into-md-txn-").tempdir_in(root)?;
    let mut staged = Vec::with_capacity(planned.len());
    for (index, (source_index, _)) in planned.iter().enumerate() {
        let mut file = context
            .temporary_file_in(stage.path(), &format!("stage-{index}"))
            .map_err(CliError::from)?;
        file.write_all_checked(&result.assets[*source_index].bytes).map_err(CliError::from)?;
        file.sync_all().map_err(CliError::from)?;
        staged.push(file.persist());
    }
    sync_directory(stage.path())?;
    Ok(StagedAssets {
        stage,
        targets: planned.into_iter().map(|(_, path)| path).collect(),
        staged,
        conflict,
        context: context.clone(),
    })
}

impl StagedAssets {
    /// Commit all staged assets after the stdout stream succeeds.
    pub fn commit(self) -> Result<Vec<WriteOutcome>, CliError> {
        let mut backups = Vec::new();
        let mut installed = Vec::new();
        for (index, (target, staged)) in self.targets.iter().zip(&self.staged).enumerate() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                rollback_output_set(&installed, &backups);
                return Err(error);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
                reject_symlink_components(parent)?;
            }
            if target.exists() {
                if self.conflict != ConflictPolicy::Overwrite {
                    rollback_output_set(&installed, &backups);
                    return Err(CliError::new(
                        ExitClass::Io,
                        "assetConflict",
                        format!("asset appeared after preflight: {}", target.display()),
                    ));
                }
                let backup = self.stage.path().join(format!("backup-{index}"));
                if let Err(error) = fs::rename(target, &backup) {
                    rollback_output_set(&installed, &backups);
                    return Err(error.into());
                }
                backups.push((target.clone(), backup));
            }
            if let Err(error) = fs::rename(staged, target) {
                rollback_output_set(&installed, &backups);
                return Err(error.into());
            }
            installed.push(target.clone());
            if let Err(error) = sync_parent(target) {
                rollback_output_set(&installed, &backups);
                return Err(error);
            }
        }
        for (_, backup) in backups {
            fs::remove_file(backup)?;
        }
        Ok(self.targets.into_iter().map(|path| WriteOutcome { path, renamed: false }).collect())
    }
}

#[cfg(test)]
fn write_assets_with_hook(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    after_preflight: impl FnOnce() -> Result<(), CliError>,
) -> Result<Vec<WriteOutcome>, CliError> {
    let planned = plan_asset_writes(result, directory, mode, conflict)?;
    if planned.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(directory)?;
    after_preflight()?;
    let mut outcomes = Vec::with_capacity(planned.len());
    for (source_index, path) in planned {
        write_exact_file(
            &path,
            &result.assets[source_index].bytes,
            conflict == ConflictPolicy::Overwrite,
        )?;
        outcomes.push(WriteOutcome { path, renamed: false });
    }
    Ok(outcomes)
}

fn plan_asset_writes(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
) -> Result<Vec<(usize, PathBuf)>, CliError> {
    if mode != AssetModeArg::Extract || result.assets.is_empty() {
        return Ok(Vec::new());
    }
    let plan = plan_assets(&result.document, &result.assets, &ConversionOptions::default())
        .map_err(CliError::from)?;
    let mut planned = Vec::with_capacity(plan.entries().len());
    let mut targets = std::collections::BTreeSet::new();
    for asset in plan.entries() {
        let path = directory.join(&asset.filename);
        if !targets.insert(path.clone()) {
            return Err(CliError::internal(format!(
                "multiple assets resolve to {}",
                path.display()
            )));
        }
        if path.exists() && conflict != ConflictPolicy::Overwrite {
            return Err(CliError::new(
                ExitClass::Io,
                "assetConflict",
                format!(
                    "stable asset output already exists and cannot be renamed safely: {}",
                    path.display()
                ),
            ));
        }
        planned.push((asset.source_index, path));
    }
    Ok(planned)
}

/// Outcome of one atomic file write.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub path: PathBuf,
    pub renamed: bool,
}

/// Atomically replace one primary artifact and all extracted resources as a set.
///
/// Every byte is staged and synced before the first target is changed. A failed
/// commit restores overwritten targets and removes targets created by this
/// transaction.
pub fn write_output_set(
    primary: &Path,
    primary_bytes: &[u8],
    result: &ConversionResult,
    asset_directory: Option<&Path>,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<WriteOutcome, CliError> {
    let primary = primary.to_path_buf();
    let planned_assets = asset_directory
        .map(|directory| plan_asset_writes(result, directory, mode, conflict))
        .transpose()?
        .unwrap_or_default();
    let mut targets = Vec::with_capacity(planned_assets.len() + 1);
    targets.push((primary.clone(), primary_bytes));
    for (source_index, path) in &planned_assets {
        targets.push((path.clone(), result.assets[*source_index].bytes.as_slice()));
    }
    commit_output_set(&targets, conflict, context)?;
    Ok(WriteOutcome { path: primary, renamed: false })
}

fn commit_output_set(
    targets: &[(PathBuf, &[u8])],
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    commit_output_set_with_hook(targets, conflict, context, |_, _| Ok(()))
}

fn commit_output_set_with_hook(
    targets: &[(PathBuf, &[u8])],
    conflict: ConflictPolicy,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let transaction_root = common_existing_ancestor(targets)?;
    reject_symlink_components(&transaction_root)?;
    ensure_same_filesystem(&transaction_root, targets)?;
    let stage = tempfile::Builder::new().prefix(".into-md-txn-").tempdir_in(&transaction_root)?;
    let mut staged = Vec::with_capacity(targets.len());
    for (index, (_, bytes)) in targets.iter().enumerate() {
        hook("stage", index)?;
        let mut file = context
            .temporary_file_in(stage.path(), &format!("stage-{index}"))
            .map_err(CliError::from)?;
        file.write_all_checked(bytes).map_err(CliError::from)?;
        hook("fsync", index)?;
        file.sync_all().map_err(CliError::from)?;
        staged.push(file.persist());
    }
    sync_directory(stage.path())?;

    let mut backups = Vec::new();
    let mut installed = Vec::new();
    for (index, ((target, _), staged_path)) in targets.iter().zip(&staged).enumerate() {
        if let Err(error) = context.checkpoint().map_err(CliError::from) {
            rollback_output_set(&installed, &backups);
            return Err(error);
        }
        if let Err(error) = hook("commit", index) {
            rollback_output_set(&installed, &backups);
            return Err(error);
        }
        if let Some(parent) = target.parent() {
            if let Err(error) = fs::create_dir_all(parent).map_err(CliError::from) {
                rollback_output_set(&installed, &backups);
                return Err(error);
            }
            if let Err(error) = reject_symlink_components(parent) {
                rollback_output_set(&installed, &backups);
                return Err(error);
            }
        }
        if target.exists() {
            if conflict != ConflictPolicy::Overwrite {
                rollback_output_set(&installed, &backups);
                return Err(CliError::new(
                    ExitClass::Io,
                    if index == 0 { "outputConflict" } else { "assetConflict" },
                    format!("output appeared after preflight: {}", target.display()),
                ));
            }
            let backup = stage.path().join(format!("backup-{index}"));
            if let Err(error) = fs::rename(target, &backup) {
                rollback_output_set(&installed, &backups);
                return Err(error.into());
            }
            backups.push((target.clone(), backup));
        }
        if let Err(error) = fs::rename(staged_path, target) {
            rollback_output_set(&installed, &backups);
            return Err(error.into());
        }
        installed.push(target.clone());
        if let Err(error) = sync_parent(target) {
            rollback_output_set(&installed, &backups);
            return Err(error);
        }
    }
    for (_, backup) in backups {
        fs::remove_file(backup)?;
    }
    sync_directory(stage.path())?;
    Ok(())
}

fn rollback_output_set(installed: &[PathBuf], backups: &[(PathBuf, PathBuf)]) {
    for target in installed.iter().rev() {
        let _ = fs::remove_file(target);
    }
    for (target, backup) in backups.iter().rev() {
        let _ = fs::rename(backup, target);
        let _ = sync_parent(target);
    }
}

fn common_existing_ancestor(targets: &[(PathBuf, &[u8])]) -> Result<PathBuf, CliError> {
    let first = targets.first().ok_or_else(|| CliError::internal("empty output transaction"))?;
    let mut candidate = first.0.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    while !candidate.exists() {
        candidate = candidate
            .parent()
            .ok_or_else(|| {
                CliError::new(ExitClass::Io, "outputPathUnsupported", "no existing output ancestor")
            })?
            .to_path_buf();
    }
    Ok(candidate)
}

fn reject_symlink_components(path: &Path) -> Result<(), CliError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(CliError::new(
                ExitClass::Io,
                "symlinkDenied",
                format!("output path contains a symbolic link: {}", current.display()),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_same_filesystem(root: &Path, targets: &[(PathBuf, &[u8])]) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt as _;
    let device = fs::metadata(root)?.dev();
    for (target, _) in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(|| {
                CliError::new(ExitClass::Io, "outputPathUnsupported", "no existing output ancestor")
            })?;
        }
        if fs::metadata(ancestor)?.dev() != device {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "an output set cannot be committed across filesystems",
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_filesystem(root: &Path, targets: &[(PathBuf, &[u8])]) -> Result<(), CliError> {
    let root = root.canonicalize()?;
    let root_prefix = root.components().next();
    for (target, _) in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(|| {
                CliError::new(ExitClass::Io, "outputPathUnsupported", "no existing output ancestor")
            })?;
        }
        let canonical = ancestor.canonicalize()?;
        if canonical.components().next() != root_prefix {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "an output set cannot be committed across filesystem roots",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), CliError> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CliError> {
    Ok(())
}

/// Write a primary artifact using the requested conflict policy.
pub fn write_file(
    requested: &Path,
    bytes: &[u8],
    conflict: ConflictPolicy,
) -> Result<WriteOutcome, CliError> {
    let (path, renamed) = resolve_conflict(requested, conflict)?;
    write_exact_file(&path, bytes, conflict == ConflictPolicy::Overwrite)?;
    Ok(WriteOutcome { path, renamed })
}

/// Resolve an output conflict without writing the file.
pub fn preflight_file(path: &Path, conflict: ConflictPolicy) -> Result<PathBuf, CliError> {
    resolve_conflict(path, conflict).map(|(resolved, _)| resolved)
}

/// Atomically write a previously resolved path without recalculating its name.
#[cfg(test)]
pub fn write_preflighted_file(
    path: &Path,
    bytes: &[u8],
    conflict: ConflictPolicy,
) -> Result<WriteOutcome, CliError> {
    write_exact_file(path, bytes, conflict == ConflictPolicy::Overwrite)?;
    Ok(WriteOutcome { path: path.to_path_buf(), renamed: false })
}

/// Write a versioned JSON report atomically.
pub fn write_report(path: &Path, report: &BatchReport) -> Result<WriteOutcome, CliError> {
    let json = report
        .to_pretty_json()
        .map_err(|error| CliError::internal(format!("serialize batch report DTO: {error}")))?;
    write_file(path, &json_with_newline(json), ConflictPolicy::Overwrite)
}

fn json_with_newline(json: String) -> Vec<u8> {
    let mut bytes = json.into_bytes();
    bytes.push(b'\n');
    bytes
}

fn encode_document(document: &into_markdown::Document) -> Result<Vec<u8>, CliError> {
    document
        .to_json()
        .map_err(|error| CliError::internal(format!("validate document IR: {error}")))?;
    let json = serde_json::to_string_pretty(document)
        .map_err(|error| CliError::internal(format!("serialize document IR: {error}")))?;
    Ok(json_with_newline(json))
}

fn encode_bundle(result: &ConversionResult) -> Result<Vec<u8>, CliError> {
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
    let assets = plan
        .entries()
        .iter()
        .map(|entry| BundleAssetDto {
            id: entry.asset_ids[0].clone(),
            source_asset_ids: entry.asset_ids.clone(),
            path: format!("assets/{}", entry.filename),
            media_type: entry.media_type.clone(),
            size: entry.size,
        })
        .collect();
    let manifest = BundleManifestDto {
        schema_version: BUNDLE_SCHEMA_VERSION,
        markdown: "document.md".into(),
        document_ir: "document.ir.json".into(),
        diagnostics: "diagnostics.json".into(),
        diagnostics_schema_version: DTO_SCHEMA_VERSION,
        provenance: "provenance.json".into(),
        provenance_schema_version: DTO_SCHEMA_VERSION,
        assets,
    };
    let manifest_json = manifest
        .to_pretty_json()
        .map(json_with_newline)
        .map_err(|error| CliError::internal(format!("serialize bundle manifest DTO: {error}")))?;
    let diagnostics = DiagnosticsDto::try_from_diagnostics(&result.diagnostics)
        .map_err(|error| CliError::internal(format!("build diagnostics DTO: {error}")))?;
    let provenance = ProvenanceListDto::try_from_provenance(&result.provenance)
        .map_err(|error| CliError::internal(format!("build provenance DTO: {error}")))?;
    let entries = [
        (
            "diagnostics.json",
            diagnostics.to_bundle_pretty_json().map(json_with_newline).map_err(|error| {
                CliError::internal(format!("serialize diagnostics DTO: {error}"))
            })?,
        ),
        ("document.ir.json", encode_document(&result.document)?),
        ("document.md", result.markdown.as_bytes().to_vec()),
        ("manifest.json", manifest_json),
        (
            "provenance.json",
            provenance.to_bundle_pretty_json().map(json_with_newline).map_err(|error| {
                CliError::internal(format!("serialize provenance DTO: {error}"))
            })?,
        ),
    ];
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut cursor);
        let file_options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        let directory_options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        for (name, bytes) in entries {
            archive
                .start_file(name, file_options)
                .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
            archive.write_all(&bytes)?;
        }
        archive.add_directory("assets/", directory_options).map_err(|error| {
            CliError::internal(format!("create bundle assets directory: {error}"))
        })?;
        for entry in plan.entries() {
            archive
                .start_file(format!("assets/{}", entry.filename), file_options)
                .map_err(|error| CliError::internal(format!("create bundle asset: {error}")))?;
            archive.write_all(&result.assets[entry.source_index].bytes)?;
        }
        archive.finish().map_err(|error| CliError::internal(format!("finish bundle: {error}")))?;
    }
    Ok(cursor.into_inner())
}

fn resolve_conflict(
    requested: &Path,
    conflict: ConflictPolicy,
) -> Result<(PathBuf, bool), CliError> {
    if !requested.exists() || conflict == ConflictPolicy::Overwrite {
        return Ok((requested.to_path_buf(), false));
    }
    if conflict == ConflictPolicy::Error {
        return Err(CliError::new(
            ExitClass::Io,
            "outputConflict",
            format!("output already exists: {}", requested.display()),
        ));
    }
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    let filename = requested.file_name().and_then(|value| value.to_str()).unwrap_or("output");
    let (stem, extension) = if let Some(stem) = filename.strip_suffix(".mdpkg.zip") {
        (stem.to_owned(), Some("mdpkg.zip".to_owned()))
    } else {
        (
            requested.file_stem().and_then(|value| value.to_str()).unwrap_or("output").to_owned(),
            requested.extension().and_then(|value| value.to_str()).map(ToOwned::to_owned),
        )
    };
    for number in 1_u64..=u64::MAX {
        let name = extension.as_ref().map_or_else(
            || format!("{stem}-{number}"),
            |extension| format!("{stem}-{number}.{extension}"),
        );
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok((candidate, true));
        }
    }
    Err(CliError::new(
        ExitClass::Io,
        "outputConflict",
        format!("could not allocate a unique output name for {}", requested.display()),
    ))
}

fn write_exact_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path.file_name().and_then(|value| value.to_str()).unwrap_or("output");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{filename}.into-md-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    let result =
        if overwrite { temporary.persist(path) } else { temporary.persist_noclobber(path) };
    result.map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            CliError::new(
                ExitClass::Io,
                "outputConflict",
                format!("output appeared after preflight: {}", path.display()),
            )
        } else {
            CliError::from(error.error)
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::{
        Asset, AssetId, Block, BlockNode, BundleManifestDto, ConversionOptions, ConversionResult,
        DiagnosticsDto, Document, NodeId, Provenance, ProvenanceKind, ProvenanceListDto,
        SourceLocator, render_markdown,
    };
    use pulldown_cmark::{Event, Parser, Tag};
    use std::io::Read as _;

    fn empty_result() -> ConversionResult {
        ConversionResult {
            document: Document::default(),
            markdown: "# Example\n".into(),
            assets: vec![Asset {
                id: AssetId("image".into()),
                filename: Some("../unsafe image.png".into()),
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            }],
            diagnostics: vec![],
            provenance: vec![],
        }
    }

    fn result_with_assets(document: Document, assets: Vec<Asset>) -> ConversionResult {
        ConversionResult {
            document,
            markdown: String::new(),
            assets,
            diagnostics: vec![],
            provenance: vec![],
        }
    }

    fn image_result(prefix: &str) -> ConversionResult {
        let asset = Asset {
            id: AssetId("bundle-image".into()),
            filename: Some("bundle image.png".into()),
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            external_uri: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: asset.id.clone(), alt: Some("bundle".into()) },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some(prefix.into());
        let markdown = render_markdown(&document, std::slice::from_ref(&asset), &options).unwrap();
        ConversionResult {
            document,
            markdown,
            assets: vec![asset],
            diagnostics: vec![],
            provenance: vec![],
        }
    }

    #[test]
    fn stable_json_envelopes_have_schema_versions() {
        let result = empty_result();
        let ir = String::from_utf8(encode_result(&result, EmitKind::IrJson).unwrap()).unwrap();
        let encoded = encode_result(&result, EmitKind::ResultJson).unwrap();
        let mut expected =
            ResultDto::json_from_result(&result, DtoJsonStyle::Pretty).unwrap().into_bytes();
        expected.push(b'\n');
        assert_eq!(encoded, expected);
        let full = String::from_utf8(encoded).unwrap();
        assert!(ir.contains("\"schemaVersion\": 1"));
        assert!(full.contains("\"dataBase64\": \"AQID\""));
    }

    #[test]
    fn cli_dto_versions_are_explicit_at_each_envelope() {
        let result = empty_result();
        let full: serde_json::Value =
            serde_json::from_slice(&encode_result(&result, EmitKind::ResultJson).unwrap()).unwrap();
        assert_eq!(full["schemaVersion"], DTO_SCHEMA_VERSION);
        assert_eq!(full["document"]["schemaVersion"], into_markdown::DOCUMENT_SCHEMA_VERSION);

        let bundle = encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bundle)).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["schemaVersion"], BUNDLE_SCHEMA_VERSION);

        let report = BatchReport::try_new(vec![]).unwrap();
        assert_eq!(report.schema_version, DTO_SCHEMA_VERSION);
    }

    #[test]
    fn bundle_contains_fixed_entries_and_safe_assets() {
        let result = empty_result();
        let bytes = encode_result(&result, EmitKind::Bundle).unwrap();
        assert_eq!(bytes, encode_result(&result, EmitKind::Bundle).unwrap());
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        assert!(names.contains(&"manifest.json".to_owned()));
        assert!(names.contains(&"assets/".to_owned()));
        let planned =
            plan_assets(&result.document, &result.assets, &ConversionOptions::default()).unwrap();
        assert!(names.contains(&format!("assets/{}", planned.entries()[0].filename)));
        assert!(!names.iter().any(|name| name.contains("..")));

        let mut manifest = String::new();
        archive.by_name("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
        let manifest = BundleManifestDto::from_json(&manifest).unwrap();
        assert_eq!(manifest.diagnostics_schema_version, DTO_SCHEMA_VERSION);
        assert_eq!(manifest.provenance_schema_version, DTO_SCHEMA_VERSION);
        let mut diagnostics = String::new();
        archive.by_name("diagnostics.json").unwrap().read_to_string(&mut diagnostics).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&diagnostics).unwrap().is_array());
        assert!(DiagnosticsDto::from_bundle_json(&diagnostics, DTO_SCHEMA_VERSION).is_ok());
        let mut provenance = String::new();
        archive.by_name("provenance.json").unwrap().read_to_string(&mut provenance).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&provenance).unwrap().is_array());
        assert!(ProvenanceListDto::from_bundle_json(&provenance, DTO_SCHEMA_VERSION).is_ok());
    }

    #[test]
    fn bundle_entries_have_fixed_file_and_directory_modes_with_or_without_assets() {
        for (has_assets, mut result) in [(false, empty_result()), (true, empty_result())] {
            if !has_assets {
                result.assets.clear();
            }
            let bytes = encode_result(&result, EmitKind::Bundle).unwrap();
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
            for index in 0..archive.len() {
                let entry = archive.by_index(index).unwrap();
                let expected = if entry.name() == "assets/" { 0o40_755 } else { 0o100_644 };
                assert_eq!(entry.unix_mode(), Some(expected), "mode for {}", entry.name());
            }
            assert_eq!(archive.by_name("assets/").unwrap().unix_mode(), Some(0o40755));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_extraction_produces_a_traversable_asset_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let result = empty_result();
        let filename = plan_assets(&result.document, &result.assets, &ConversionOptions::default())
            .unwrap()
            .entries()[0]
            .filename
            .clone();
        let bytes = encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let destination = tempfile::tempdir().unwrap();
        archive.extract(destination.path()).unwrap();

        let assets = destination.path().join("assets");
        assert_eq!(fs::metadata(&assets).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(
            fs::metadata(assets.join(&filename)).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(assets.join(filename)).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn bundle_uses_stable_cross_platform_asset_names() {
        let mut result = empty_result();
        result.assets = vec![
            Asset {
                id: AssetId("upper".into()),
                filename: Some("Image.png".into()),
                media_type: "image/png".into(),
                bytes: vec![1],
                external_uri: None,
            },
            Asset {
                id: AssetId("lower".into()),
                filename: Some("image.png".into()),
                media_type: "image/png".into(),
                bytes: vec![2],
                external_uri: None,
            },
            Asset {
                id: AssetId("reserved".into()),
                filename: Some("CON.txt".into()),
                media_type: "text/plain".into(),
                bytes: vec![3],
                external_uri: None,
            },
            Asset {
                id: AssetId("unicode".into()),
                filename: Some("图片.png".into()),
                media_type: "image/png".into(),
                bytes: vec![4],
                external_uri: None,
            },
        ];
        let bytes = encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        let plan =
            plan_assets(&result.document, &result.assets, &ConversionOptions::default()).unwrap();
        for asset in plan.entries() {
            assert!(names.contains(&format!("assets/{}", asset.filename)));
        }
        let asset_entries = names
            .iter()
            .filter(|name| name.starts_with("assets/") && name.as_str() != "assets/")
            .collect::<Vec<_>>();
        assert_eq!(asset_entries.len(), result.assets.len());
        assert!(asset_entries.iter().all(|name| name.is_ascii()));
    }

    #[test]
    fn report_writer_uses_the_public_batch_contract() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        let report = BatchReport::try_new(vec![BatchItemReport {
            input: "example.txt".into(),
            output: Some("example.md".into()),
            format: Some("text".into()),
            status: BatchItemStatus::Success,
            diagnostics: vec![],
            error_code: None,
            message: None,
            warnings: vec![],
        }])
        .unwrap();
        write_report(&path, &report).unwrap();
        let json = fs::read_to_string(path).unwrap();
        assert_eq!(BatchReport::from_json(&json).unwrap(), report);
    }

    #[test]
    fn bundle_markdown_image_href_exactly_matches_its_zip_entry() {
        let result = image_result("assets");
        let bytes = encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let markdown = {
            let mut entry = archive.by_name("document.md").unwrap();
            let mut markdown = String::new();
            entry.read_to_string(&mut markdown).unwrap();
            markdown
        };
        let href = Parser::new(&markdown)
            .find_map(|event| match event {
                Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            })
            .unwrap();
        assert!(archive.by_name(&href).is_ok(), "missing ZIP entry for image href {href}");
        let plan =
            plan_assets(&result.document, &result.assets, &ConversionOptions::default()).unwrap();
        assert_eq!(href, format!("assets/{}", plan.entries()[0].filename));
    }

    #[test]
    fn stable_asset_conflicts_are_preflighted_before_any_write() {
        let root = std::env::temp_dir().join(format!(
            "into-md-assets-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let assets = vec![
            Asset {
                id: AssetId("first".into()),
                filename: Some("same.PNG".into()),
                media_type: "image/png".into(),
                bytes: vec![1],
                external_uri: None,
            },
            Asset {
                id: AssetId("second".into()),
                filename: Some("same.png".into()),
                media_type: "image/png".into(),
                bytes: vec![2],
                external_uri: None,
            },
        ];
        let result = result_with_assets(Document::default(), assets);
        let planned =
            plan_assets(&result.document, &result.assets, &ConversionOptions::default()).unwrap();
        let second = root.join(planned.uri("second").unwrap());
        fs::write(&second, b"existing").unwrap();
        let error = write_assets(&result, &root, AssetModeArg::Extract, ConflictPolicy::Rename)
            .unwrap_err();
        assert_eq!(error.code(), "assetConflict");
        assert!(!root.join(planned.uri("first").unwrap()).exists());
        assert_eq!(fs::read(second).unwrap(), b"existing");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_preflight_races_never_overwrite_primary_or_asset_targets() {
        let root = std::env::temp_dir().join(format!(
            "into-md-race-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let requested = root.join("document.md");
        fs::write(&requested, b"original").unwrap();
        let planned = preflight_file(&requested, ConflictPolicy::Rename).unwrap();
        assert_eq!(planned, root.join("document-1.md"));
        fs::write(&planned, b"racer").unwrap();
        let error = write_preflighted_file(&planned, b"new", ConflictPolicy::Rename).unwrap_err();
        assert_eq!(error.code(), "outputConflict");
        assert_eq!(fs::read(&planned).unwrap(), b"racer");

        let asset = Asset {
            id: AssetId("race".into()),
            filename: Some("race.png".into()),
            media_type: "image/png".into(),
            bytes: vec![9],
            external_uri: None,
        };
        let result = result_with_assets(Document::default(), vec![asset]);
        let asset_target = root.join(
            &plan_assets(&result.document, &result.assets, &ConversionOptions::default())
                .unwrap()
                .entries()[0]
                .filename,
        );
        let error = write_assets_with_hook(
            &result,
            &root,
            AssetModeArg::Extract,
            ConflictPolicy::Rename,
            || {
                fs::write(&asset_target, b"asset-racer")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputConflict");
        assert_eq!(fs::read(asset_target).unwrap(), b"asset-racer");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renderer_asset_uri_and_writer_target_use_the_same_plan() {
        let root = std::env::temp_dir().join(format!(
            "into-md-linked-assets-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        let asset = Asset {
            id: AssetId("图片/CON".into()),
            filename: Some("dir\\Case.PNG".into()),
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            external_uri: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: asset.id.clone(), alt: Some("image".into()) },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some("assets".into());
        let markdown = render_markdown(&document, std::slice::from_ref(&asset), &options).unwrap();
        let filename =
            plan_assets(&document, std::slice::from_ref(&asset), &options).unwrap().entries()[0]
                .filename
                .clone();
        assert!(markdown.contains(&format!("assets/{filename}")));
        let result = result_with_assets(document, vec![asset]);
        write_assets(&result, &root.join("assets"), AssetModeArg::Extract, ConflictPolicy::Error)
            .unwrap();
        assert_eq!(fs::read(root.join("assets").join(filename)).unwrap(), [1, 2, 3]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conflict_renaming_is_deterministic() {
        let root = std::env::temp_dir().join(format!("into-md-output-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let requested = root.join("document.md");
        fs::write(&requested, "existing").unwrap();
        let outcome = write_file(&requested, b"new", ConflictPolicy::Rename).unwrap();
        assert_eq!(outcome.path, root.join("document-1.md"));
        assert!(outcome.renamed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_and_commit_failures_leave_old_output_set_intact() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = root.path().canonicalize().unwrap();
        let primary = canonical_root.join("document.md");
        let asset = canonical_root.join("assets/image.png");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        fs::write(&primary, b"old-document").unwrap();
        fs::write(&asset, b"old-asset").unwrap();
        let targets = vec![
            (primary.clone(), b"new-document".as_slice()),
            (asset.clone(), b"new-asset".as_slice()),
        ];
        let context = ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );

        let error = commit_output_set_with_hook(
            &targets,
            ConflictPolicy::Overwrite,
            &context,
            |phase, index| {
                if phase == "stage" && index == 1 {
                    Err(CliError::new(ExitClass::Io, "injectedStageFailure", "test"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "injectedStageFailure");
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert_eq!(fs::read(&asset).unwrap(), b"old-asset");

        let error = commit_output_set_with_hook(
            &targets,
            ConflictPolicy::Overwrite,
            &context,
            |phase, index| {
                if phase == "commit" && index == 1 {
                    Err(CliError::new(ExitClass::Io, "injectedCommitFailure", "test"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "injectedCommitFailure");
        assert_eq!(fs::read(primary).unwrap(), b"old-document");
        assert_eq!(fs::read(asset).unwrap(), b"old-asset");

        let limits = into_markdown::ResourceLimits { max_temporary_bytes: 5, ..Default::default() };
        let limited = ExecutionContext::new(into_markdown::ExecutionOptions::default(), limits);
        let error = commit_output_set(&targets, ConflictPolicy::Overwrite, &limited).unwrap_err();
        assert_eq!(error.code(), "resourceLimit");

        let token = into_markdown::CancellationToken::new();
        let cancelled = ExecutionContext::new(
            into_markdown::ExecutionOptions { cancellation: token.clone(), ..Default::default() },
            into_markdown::ResourceLimits::default(),
        );
        token.cancel();
        let error = commit_output_set(&targets, ConflictPolicy::Overwrite, &cancelled).unwrap_err();
        assert_eq!(error.code(), "cancelled");
        assert_eq!(fs::read(&targets[0].0).unwrap(), b"old-document");
        assert_eq!(fs::read(&targets[1].0).unwrap(), b"old-asset");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swap_is_rejected_and_rolls_back_primary() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let primary = root.join("document.md");
        let asset_parent = root.join("assets");
        let attacker = root.join("attacker");
        fs::create_dir_all(&asset_parent).unwrap();
        fs::create_dir_all(&attacker).unwrap();
        fs::write(&primary, b"old").unwrap();
        let asset = asset_parent.join("image.png");
        let targets =
            vec![(primary.clone(), b"new".as_slice()), (asset.clone(), b"image".as_slice())];
        let context = ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );
        let error = commit_output_set_with_hook(
            &targets,
            ConflictPolicy::Overwrite,
            &context,
            |phase, index| {
                if phase == "commit" && index == 1 {
                    fs::remove_dir(&asset_parent)?;
                    symlink(&attacker, &asset_parent)?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "symlinkDenied");
        assert_eq!(fs::read(primary).unwrap(), b"old");
        assert!(!attacker.join("image.png").exists());
    }
}
