//! Stable CLI serialization, bundles, batch reports, and atomic output writes.

use crate::args::{AssetModeArg, ConflictPolicy, EmitKind};
use crate::error::{CliError, ExitClass};
use base64::Engine as _;
use into_markdown::{Asset, ConversionResult, Diagnostic, Document, Provenance};
use serde::Serialize;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

// These wire protocols evolve independently from the Document IR schema.
const RESULT_DOCUMENT_SCHEMA_VERSION: u32 = 1;
const BUNDLE_MANIFEST_SCHEMA_VERSION: u32 = 1;
const BATCH_REPORT_SCHEMA_VERSION: u32 = 1;

/// Versioned conversion result transport envelope.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultDocument<'a> {
    schema_version: u32,
    markdown: &'a str,
    document: &'a Document,
    assets: Vec<JsonAsset<'a>>,
    diagnostics: &'a [Diagnostic],
    provenance: &'a [Provenance],
}

/// JSON representation of one asset with base64 content.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonAsset<'a> {
    id: &'a str,
    filename: &'a Option<String>,
    media_type: &'a str,
    data_base64: String,
    external_uri: &'a Option<String>,
}

/// Portable bundle manifest.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleManifest {
    schema_version: u32,
    markdown: &'static str,
    document_ir: &'static str,
    diagnostics: &'static str,
    provenance: &'static str,
    assets: Vec<BundleAsset>,
}

/// One bundle asset entry.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleAsset {
    id: String,
    path: String,
    media_type: String,
    size: usize,
}

/// One batch report item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItemReport {
    pub input: String,
    pub output: Option<String>,
    pub format: Option<String>,
    pub status: String,
    pub diagnostics: Vec<Diagnostic>,
    pub error_code: Option<String>,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

/// Versioned machine-readable batch report.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchReport {
    pub schema_version: u32,
    pub succeeded: usize,
    pub failed: usize,
    pub items: Vec<BatchItemReport>,
}

impl BatchReport {
    /// Build a deterministic report from ordered items.
    pub fn new(items: Vec<BatchItemReport>) -> Self {
        let succeeded = items.iter().filter(|item| item.status == "success").count();
        let failed = items.iter().filter(|item| item.status == "failed").count();
        Self { schema_version: BATCH_REPORT_SCHEMA_VERSION, succeeded, failed, items }
    }
}

/// Serialize a conversion result into the selected primary artifact.
pub fn encode_result(result: &ConversionResult, emit: EmitKind) -> Result<Vec<u8>, CliError> {
    match emit {
        EmitKind::Markdown => Ok(result.markdown.as_bytes().to_vec()),
        EmitKind::IrJson => pretty_json(&result.document),
        EmitKind::ResultJson => pretty_json(&ResultDocument {
            schema_version: RESULT_DOCUMENT_SCHEMA_VERSION,
            markdown: &result.markdown,
            document: &result.document,
            assets: result
                .assets
                .iter()
                .map(|asset| JsonAsset {
                    id: &asset.id.0,
                    filename: &asset.filename,
                    media_type: &asset.media_type,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&asset.bytes),
                    external_uri: &asset.external_uri,
                })
                .collect(),
            diagnostics: &result.diagnostics,
            provenance: &result.provenance,
        }),
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
    write_file(path, &pretty_json(report)?, ConflictPolicy::Overwrite)
}

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>, CliError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| CliError::internal(format!("serialize JSON: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
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
        assets.push(BundleAsset {
            id: asset.id.0.clone(),
            path,
            media_type: asset.media_type.clone(),
            size: asset.bytes.len(),
        });
    }
    let manifest = BundleManifest {
        schema_version: BUNDLE_MANIFEST_SCHEMA_VERSION,
        markdown: "document.md",
        document_ir: "document.ir.json",
        diagnostics: "diagnostics.json",
        provenance: "provenance.json",
        assets,
    };
    let entries = [
        ("diagnostics.json", pretty_json(&result.diagnostics)?),
        ("document.ir.json", pretty_json(&result.document)?),
        ("document.md", result.markdown.as_bytes().to_vec()),
        ("manifest.json", pretty_json(&manifest)?),
        ("provenance.json", pretty_json(&result.provenance)?),
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
    if !existing.iter().any(|value| value == requested) {
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
        if !existing.iter().any(|value| value == &candidate) {
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
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() || matches!(sanitized.as_str(), "." | "..") {
        "asset".into()
    } else {
        sanitized
    }
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
    use into_markdown::{AssetId, ConversionResult, Document};

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
    fn cli_dto_versions_are_independent_from_document_ir_version() {
        let mut result = empty_result();
        result.document.schema_version = 27;
        let full: serde_json::Value =
            serde_json::from_slice(&encode_result(&result, EmitKind::ResultJson).unwrap()).unwrap();
        assert_eq!(full["schemaVersion"], RESULT_DOCUMENT_SCHEMA_VERSION);
        assert_eq!(full["document"]["schemaVersion"], 27);

        let bundle = encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bundle)).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["schemaVersion"], BUNDLE_MANIFEST_SCHEMA_VERSION);

        let report = BatchReport::new(vec![]);
        assert_eq!(report.schema_version, BATCH_REPORT_SCHEMA_VERSION);
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
        assert!(names.contains(&"assets/unsafe_image.png".to_owned()));
        assert!(!names.iter().any(|name| name.contains("..")));
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
