//! Stable CLI artifact streaming, serialization, and atomic publication.

mod assets;
mod bundle;
mod commit;
mod serialization;
mod stdout;
mod stream;

#[cfg(test)]
pub(crate) use assets::stage_assets;
pub(crate) use assets::stage_spooled_assets;
#[cfg(test)]
pub(crate) use assets::write_assets;
#[cfg(test)]
use assets::{plan_asset_writes, write_assets_with_hook};
pub(crate) use bundle::write_bundle;
#[cfg(test)]
use commit::write_preflighted_file;
pub(crate) use commit::{preflight_file, write_file, write_report, write_spooled_output_set_file};
pub(crate) use serialization::encode_result;
pub(crate) use stdout::publish as publish_stdout;
pub(crate) use stream::StructuredSpool;

#[cfg(test)]
use crate::args::{AssetModeArg, ConflictPolicy, EmitKind};
use into_markdown::BatchReportDto;
#[cfg(test)]
use into_markdown::{
    BUNDLE_SCHEMA_VERSION, DTO_SCHEMA_VERSION, DtoJsonStyle, ExecutionContext, ResultDto,
    plan_assets,
};
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::io::Cursor;

pub use into_markdown::{
    BatchItemDto as BatchItemReport, BatchItemOutcome, BatchItemStatus, BatchLimitDto,
};
pub type BatchReport = BatchReportDto;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::transaction::{self, Target};
    use into_markdown::{
        Asset, AssetId, Block, BlockNode, BundleManifestDto, ConversionOptions, ConversionResult,
        DiagnosticsDto, Document, NodeId, Provenance, ProvenanceKind, ProvenanceListDto,
        SourceLocator, render_markdown,
    };
    use pulldown_cmark::{Event, Parser, Tag};
    use std::io::Read as _;
    #[cfg(unix)]
    use std::path::Path;

    fn empty_result() -> ConversionResult {
        ConversionResult::new(
            Document::default(),
            "# Example\n".into(),
            vec![Asset {
                id: AssetId("image".into()),
                filename: Some("../unsafe image.png".into()),
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            }],
            vec![],
            vec![],
        )
    }

    fn result_with_assets(document: Document, assets: Vec<Asset>) -> ConversionResult {
        ConversionResult::new(document, String::new(), assets, vec![], vec![])
    }

    fn output_context() -> ExecutionContext {
        ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        )
    }

    #[cfg(unix)]
    fn leave_installed_residue(path: &Path, bytes: &[u8], overwrite: bool) {
        let context = output_context();
        let targets = [Target { path: path.to_path_buf(), bytes }];
        let mut transaction = transaction::prepare(&targets, overwrite, &context).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(transaction::HookDecision::SimulateCrash)
                } else {
                    Ok(transaction::HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);
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
        ConversionResult::new(document, markdown, vec![asset], vec![], vec![])
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
        let path = directory.path().canonicalize().unwrap().join("report.json");
        let report = BatchReport::try_new(vec![BatchItemReport {
            input: "example.txt".into(),
            output: Some("example.md".into()),
            format: Some("text".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
        }])
        .unwrap();
        write_report(&path, &report, &output_context()).unwrap();
        let json = fs::read_to_string(path).unwrap();
        assert_eq!(BatchReport::from_json(&json).unwrap(), report);
    }

    #[cfg(unix)]
    #[test]
    fn report_writer_recovers_an_interrupted_output_before_replacing_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("report.json");
        fs::write(&path, b"old-report").unwrap();
        leave_installed_residue(&path, b"interrupted-report", true);

        let report = BatchReport::try_new(vec![]).unwrap();
        write_report(&path, &report, &output_context()).unwrap();
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
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
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
    }

    #[test]
    fn post_preflight_races_never_overwrite_primary_or_asset_targets() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();

        let requested = root.join("document.md");
        fs::write(&requested, b"original").unwrap();
        let context = output_context();
        let planned = preflight_file(&requested, ConflictPolicy::Rename, &context).unwrap();
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
    }

    #[test]
    fn renderer_asset_uri_and_writer_target_use_the_same_plan() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
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
    }

    #[test]
    fn conflict_renaming_is_deterministic() {
        let root = std::env::temp_dir().join(format!("into-md-output-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let requested = root.join("document.md");
        fs::write(&requested, "existing").unwrap();
        let outcome =
            write_file(&requested, b"new", ConflictPolicy::Rename, &output_context()).unwrap();
        assert_eq!(outcome.path, root.join("document-1.md"));
        assert!(outcome.renamed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stdout_assets_use_the_shared_transaction_for_commit_and_abort() {
        let temporary = tempfile::tempdir().unwrap();
        let assets = temporary.path().canonicalize().unwrap().join("assets");
        fs::create_dir(&assets).unwrap();
        let result = empty_result();
        let planned = plan_asset_writes(
            &result,
            &assets,
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            None,
        )
        .unwrap();
        let target = planned[0].1.clone();
        fs::write(&target, b"old").unwrap();
        let context = output_context();

        let staged = stage_assets(
            &result,
            &assets,
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            &context,
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        staged.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), [1, 2, 3]);

        let staged = stage_assets(
            &result,
            &assets,
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            &context,
        )
        .unwrap();
        staged.abort().unwrap();
        assert_eq!(fs::read(&target).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn spooled_primary_and_assets_publish_as_one_atomic_file_set() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let assets = root.join("assets");
        fs::create_dir(&assets).unwrap();
        let primary = root.join("document.md");
        let context = output_context();
        let result = empty_result();
        let mut spool = StructuredSpool::from_result(
            &result,
            context.clone(),
            EmitKind::Markdown,
            AssetModeArg::Extract,
        )
        .unwrap();
        let asset_name = spool.external_payloads().unwrap()[0].0.to_owned();
        let asset_target = assets.join(asset_name);
        fs::write(&asset_target, b"old-asset").unwrap();
        let mut encoded = context.temporary_file_in(&root, "encoded").unwrap();
        spool.serialize(EmitKind::Markdown, &mut encoded).unwrap();
        encoded.sync_all().unwrap();

        let error = write_spooled_output_set_file(
            &primary,
            encoded.as_file().unwrap(),
            &spool,
            Some(&assets),
            AssetModeArg::Extract,
            ConflictPolicy::Error,
            &context,
        )
        .unwrap_err();
        assert_eq!(error.code(), "assetConflict");
        assert!(!primary.exists());
        assert_eq!(fs::read(&asset_target).unwrap(), b"old-asset");

        write_spooled_output_set_file(
            &primary,
            encoded.as_file().unwrap(),
            &spool,
            Some(&assets),
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            &context,
        )
        .unwrap();
        assert_eq!(fs::read(primary).unwrap(), result.markdown.as_bytes());
        assert_eq!(fs::read(asset_target).unwrap(), [1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn stdout_asset_staging_recovers_before_the_stream_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let assets = temporary.path().canonicalize().unwrap().join("assets");
        fs::create_dir(&assets).unwrap();
        let result = empty_result();
        let planned = plan_asset_writes(
            &result,
            &assets,
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            None,
        )
        .unwrap();
        let target = planned[0].1.clone();
        fs::write(&target, b"old-asset").unwrap();
        leave_installed_residue(&target, b"interrupted-asset", true);

        let staged = stage_assets(
            &result,
            &assets,
            AssetModeArg::Extract,
            ConflictPolicy::Overwrite,
            &output_context(),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old-asset");
        staged.commit().unwrap();
        assert_eq!(fs::read(target).unwrap(), [1, 2, 3]);
    }
}
