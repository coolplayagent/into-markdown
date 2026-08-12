//! Stable CLI serialization, bundles, batch reports, and atomic output writes.

use crate::args::{AssetModeArg, ConflictPolicy, EmitKind};
use crate::error::{CliError, ExitClass};
use into_markdown::{
    Asset, BatchReportDto, BundleAssetDto, BundleManifestDto, ConversionResult, DTO_SCHEMA_VERSION,
    DiagnosticsDto, ProvenanceListDto, ResultDto,
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
            let dto = ResultDto::try_from(result)
                .map_err(|error| CliError::internal(format!("build result DTO: {error}")))?;
            dto.to_pretty_json()
                .map(json_with_newline)
                .map_err(|error| CliError::internal(format!("serialize result DTO: {error}")))
        }
        EmitKind::Bundle => encode_bundle(result),
    }
}

/// Write extracted assets using safe, deterministic filenames.
pub fn write_assets(
    assets: &[Asset],
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
) -> Result<Vec<WriteOutcome>, CliError> {
    if mode != AssetModeArg::Extract || assets.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(directory)?;
    let mut outcomes = Vec::with_capacity(assets.len());
    for (index, asset) in assets.iter().enumerate() {
        if asset.bytes.is_empty() {
            continue;
        }
        let fallback = format!("asset-{}", index + 1);
        let filename = sanitize_filename(asset.filename.as_deref().unwrap_or(&fallback));
        outcomes.push(write_file(&directory.join(filename), &asset.bytes, conflict)?);
    }
    Ok(outcomes)
}

/// Outcome of one atomic file write.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub path: PathBuf,
    pub renamed: bool,
}

/// Write a primary artifact using the requested conflict policy.
pub fn write_file(
    requested: &Path,
    bytes: &[u8],
    conflict: ConflictPolicy,
) -> Result<WriteOutcome, CliError> {
    let (path, renamed) = resolve_conflict(requested, conflict)?;
    atomic_write(&path, bytes)?;
    Ok(WriteOutcome { path, renamed })
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
    let mut assets = Vec::new();
    let mut asset_entries = Vec::new();
    for (index, asset) in result.assets.iter().enumerate() {
        if asset.bytes.is_empty() {
            continue;
        }
        let fallback = format!("asset-{}", index + 1);
        let filename = sanitize_filename(asset.filename.as_deref().unwrap_or(&fallback));
        let path = unique_bundle_path(&asset_entries, &format!("assets/{filename}"));
        asset_entries.push(path.clone());
        assets.push(BundleAssetDto {
            id: asset.id.0.clone(),
            path,
            media_type: asset.media_type.clone(),
            size: u64::try_from(asset.bytes.len()).map_err(|_| {
                CliError::internal(format!(
                    "asset {} size cannot be represented by the DTO",
                    asset.id.0
                ))
            })?,
        });
    }
    let manifest = BundleManifestDto {
        schema_version: DTO_SCHEMA_VERSION,
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
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, bytes) in entries {
            archive
                .start_file(name, options)
                .map_err(|error| CliError::internal(format!("create bundle entry: {error}")))?;
            archive.write_all(&bytes)?;
        }
        archive.add_directory("assets/", options).map_err(|error| {
            CliError::internal(format!("create bundle assets directory: {error}"))
        })?;
        for (asset, entry) in
            result.assets.iter().filter(|asset| !asset.bytes.is_empty()).zip(asset_entries)
        {
            archive
                .start_file(entry, options)
                .map_err(|error| CliError::internal(format!("create bundle asset: {error}")))?;
            archive.write_all(&asset.bytes)?;
        }
        archive.finish().map_err(|error| CliError::internal(format!("finish bundle: {error}")))?;
    }
    Ok(cursor.into_inner())
}

fn unique_bundle_path(existing: &[String], requested: &str) -> String {
    if !existing.iter().any(|value| value.eq_ignore_ascii_case(requested)) {
        return requested.to_owned();
    }
    let path = Path::new(requested);
    let parent = path.parent().and_then(Path::to_str).unwrap_or("");
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or("asset");
    let extension = path.extension().and_then(|value| value.to_str());
    for number in 1_u64..=u64::MAX {
        let name = extension.map_or_else(
            || format!("{stem}-{number}"),
            |extension| format!("{stem}-{number}.{extension}"),
        );
        let candidate = if parent.is_empty() { name } else { format!("{parent}/{name}") };
        if !existing.iter().any(|value| value.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    format!("assets/asset-{}", existing.len() + 1)
}

fn sanitize_filename(value: &str) -> String {
    let filename = Path::new(value).file_name().and_then(|part| part.to_str()).unwrap_or("asset");
    let sanitized = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim_end_matches(['.', ' ']);
    if sanitized.is_empty() || matches!(sanitized, "." | "..") {
        "asset".into()
    } else if is_windows_reserved_filename(sanitized) {
        format!("_{sanitized}")
    } else {
        sanitized.into()
    }
}

fn is_windows_reserved_filename(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
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

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path.file_name().and_then(|value| value.to_str()).unwrap_or("output");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{filename}.into-md-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| CliError::from(error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::{
        AssetId, BundleManifestDto, ConversionResult, DiagnosticsDto, Document, ProvenanceListDto,
    };
    use std::io::Read;

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

    #[test]
    fn stable_json_envelopes_have_schema_versions() {
        let result = empty_result();
        let ir = String::from_utf8(encode_result(&result, EmitKind::IrJson).unwrap()).unwrap();
        let full =
            String::from_utf8(encode_result(&result, EmitKind::ResultJson).unwrap()).unwrap();
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
        assert_eq!(manifest["schemaVersion"], DTO_SCHEMA_VERSION);

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
        assert!(names.contains(&"assets/unsafe_image.png".to_owned()));
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
    fn bundle_renames_case_collisions_and_reserved_asset_names() {
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
        assert!(names.contains(&"assets/Image.png".into()));
        assert!(names.contains(&"assets/image-1.png".into()));
        assert!(names.contains(&"assets/_CON.txt".into()));
        assert!(names.contains(&"assets/__.png".into()));
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
}
