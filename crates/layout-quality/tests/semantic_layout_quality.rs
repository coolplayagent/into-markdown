//! Cross-format semantic layout, reading-order, and rendering quality gate.

use into_markdown_converters::{
    DocxConverter, EpubConverter, HtmlConverter, MsgConverter, OdfConverter, PresentationConverter,
    RtfConverter, WorkbookConverter,
};
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    ExecutionOptions, FormatCandidate, InputFormat, NestedConversionRequest,
    NestedConversionService, ResolvedInput, Services, SourceMetadata,
};
use into_markdown_layout_quality::{
    AUTHORITY_SCHEMA_VERSION, FixtureAuthority, QualityCohort, audit, project,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CASES: &[&str] = &[
    "docx-normal",
    "docx-malicious",
    "pptx-normal",
    "pptm-malicious",
    "ppsx-normal",
    "potx-normal",
    "xlsx-normal",
    "xlsm-normal",
    "xlsb-normal",
    "odt-normal",
    "odt-implicit-nested-list",
    "ods-normal",
    "ods-span-nested",
    "odp-normal",
    "odp-rotation",
    "rtf-normal",
    "rtf-malicious",
    "epub-normal",
    "msg-normal",
    "msg-cid",
    "msg-attachment-nested",
];

const NEGATIVE_CASES: &[(&str, &str)] = &[
    ("docx-corrupt", "docx-limit"),
    ("pptx-corrupt", "pptx-limit"),
    ("xlsb-corrupt", "xlsb-limit"),
    ("odt-corrupt", "odt-limit"),
    ("ods-corrupt", "ods-limit"),
    ("odp-corrupt", "odp-limit"),
    ("rtf-corrupt", "rtf-limit"),
    ("msg-corrupt", "msg-limit"),
];

#[derive(Debug, Deserialize)]
struct Manifest {
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    format: String,
    path: String,
    bytes: u64,
    sha256: String,
    media_type: String,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    error_code: String,
    #[serde(default)]
    limit: Option<Limit>,
}

#[derive(Debug, Deserialize)]
struct Limit {
    option: String,
    failing_value: u64,
    passing_value: u64,
    #[serde(rename = "error_limit")]
    reported_name: String,
    passing_semantic_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityBundle {
    schema_version: u32,
    fixture_manifest_sha256: String,
    authorities: Vec<FixtureAuthority>,
    coverage: Vec<FamilyCoverage>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyCoverage {
    format_family: String,
    normal_fixture: String,
    complex_fixture: String,
    misordered_counterexample: String,
    corrupt_fixture: String,
    resource_boundary_fixture: String,
    gate: String,
}

#[test]
fn real_converters_match_hash_pinned_ir_gfm_and_semantic_goldens_twice() {
    let (manifest_bytes, manifest) = manifest();
    let bundle = authority_bundle();
    assert_eq!(bundle.schema_version, AUTHORITY_SCHEMA_VERSION);
    assert_eq!(bundle.fixture_manifest_sha256, sha256(&manifest_bytes));
    let authorities = bundle
        .authorities
        .iter()
        .map(|authority| (authority.fixture_id.as_str(), authority))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(authorities.len(), CASES.len(), "quality case inventory drift");
    let mut failures = Vec::new();
    for fixture_id in CASES {
        let fixture = fixture(&manifest, fixture_id);
        let authority = authorities
            .get(fixture_id)
            .unwrap_or_else(|| panic!("missing authority for {fixture_id}"));
        assert_eq!(authority.format, fixture.format, "{fixture_id} format drift");
        let first = convert(fixture).unwrap_or_else(|error| panic!("{fixture_id}: {error}"));
        let second = convert(fixture)
            .unwrap_or_else(|error| panic!("{fixture_id} repeated conversion: {error}"));
        assert_eq!(first.document.to_json().unwrap(), second.document.to_json().unwrap());
        assert_eq!(first.assets, second.assets, "{fixture_id} asset drift");
        assert_eq!(first.markdown, second.markdown, "{fixture_id} GFM drift");
        let context =
            ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
        match audit(authority, &first.document, &first.assets, &first.markdown, &context) {
            Ok(report) if report.passed => {
                assert_eq!(report.metrics.precision_basis_points, 10_000);
                assert_eq!(report.metrics.recall_basis_points, 10_000);
            }
            Ok(report) => failures.extend(report.diffs.into_iter().map(|diff| {
                format!(
                    "{} {:?} {} expected={:?} actual={:?}",
                    diff.fixture_id, diff.kind, diff.location, diff.expected, diff.actual
                )
            })),
            Err(error) => failures.push(format!("{fixture_id} audit failed: {error}")),
        }
    }
    assert!(failures.is_empty(), "semantic layout failures:\n{}", failures.join("\n"));
}

#[test]
fn every_core_family_has_normal_complex_order_corrupt_and_boundary_authority() {
    let (_, manifest) = manifest();
    let bundle = authority_bundle();
    let fixture_ids =
        manifest.fixtures.iter().map(|fixture| fixture.id.as_str()).collect::<BTreeSet<_>>();
    let required = BTreeSet::from([
        "wordprocessingml-docx-docm",
        "presentationml",
        "spreadsheetml-xlsb",
        "odf-text",
        "odf-presentation",
        "odf-spreadsheet",
        "rtf",
        "epub",
        "pdf",
        "image-ocr",
        "outlook-msg",
        "legacy-office",
    ]);
    let actual =
        bundle.coverage.iter().map(|item| item.format_family.as_str()).collect::<BTreeSet<_>>();
    assert_eq!(actual, required, "format-family coverage drift");
    for item in &bundle.coverage {
        for reference in [
            &item.normal_fixture,
            &item.complex_fixture,
            &item.misordered_counterexample,
            &item.corrupt_fixture,
            &item.resource_boundary_fixture,
        ] {
            assert!(
                fixture_ids.contains(reference.as_str())
                    || reference.starts_with("counterexample:")
                    || reference.starts_with("gate:"),
                "{} has an unknown coverage reference {reference}",
                item.format_family
            );
        }
        assert!(item.misordered_counterexample.starts_with("counterexample:"));
        assert!(item.gate.starts_with("//"), "{} gate is not a Bazel target", item.format_family);
    }
}

#[test]
fn real_corrupt_and_resource_boundary_fixtures_fail_with_typed_errors_and_exact_limits() {
    let (_, manifest) = manifest();
    for (corrupt_id, limit_id) in NEGATIVE_CASES {
        let corrupt = fixture(&manifest, corrupt_id);
        let error = convert(corrupt).expect_err(corrupt_id);
        assert_eq!(error.code().as_str(), corrupt.expected.error_code, "{corrupt_id}: {error}");

        let limited = fixture(&manifest, limit_id);
        let limit = limited.expected.limit.as_ref().unwrap_or_else(|| panic!("{limit_id}"));
        let error = convert_with_limit(limited, Some((&limit.option, limit.failing_value)))
            .expect_err(limit_id);
        assert_eq!(error.code().as_str(), "resourceLimit", "{limit_id}: {error}");
        match error {
            ConversionError::ResourceLimit { limit: actual, .. } => {
                assert_eq!(actual, limit.reported_name, "{limit_id}");
            }
            other => panic!("{limit_id}: {other}"),
        }
        let passing = convert_with_limit(limited, Some((&limit.option, limit.passing_value)))
            .unwrap_or_else(|error| panic!("{limit_id} passing boundary: {error}"));
        assert_eq!(
            sha256(passing.markdown.as_bytes()),
            limit.passing_semantic_sha256,
            "{limit_id} passing GFM drift"
        );
    }
}

#[test]
#[ignore = "review the generated authority diff before replacing the checked-in file"]
fn generate_review_candidate() {
    let output = std::env::var_os("SEMANTIC_AUTHORITY_OUTPUT")
        .expect("set SEMANTIC_AUTHORITY_OUTPUT to an explicit temporary path");
    let (manifest_bytes, manifest) = manifest();
    let mut authorities = Vec::new();
    for fixture_id in CASES {
        let fixture = fixture(&manifest, fixture_id);
        let converted = convert(fixture).unwrap();
        let context =
            ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
        authorities.push(FixtureAuthority {
            schema_version: AUTHORITY_SCHEMA_VERSION,
            fixture_id: fixture.id.clone(),
            format: fixture.format.clone(),
            cohort: QualityCohort::Modern,
            geometry_tolerance_milli: 0,
            snapshot: project(&converted.document, &converted.assets, &context)
                .unwrap()
                .into_authority_snapshot(),
            ir_sha256: sha256(&serde_json::to_vec(&converted.document).unwrap()),
            gfm_sha256: sha256(converted.markdown.as_bytes()),
        });
    }
    let bundle = AuthorityBundle {
        schema_version: AUTHORITY_SCHEMA_VERSION,
        fixture_manifest_sha256: sha256(&manifest_bytes),
        authorities,
        coverage: coverage(),
    };
    let mut bytes = serde_json::to_vec_pretty(&bundle).unwrap();
    bytes.push(b'\n');
    std::fs::write(PathBuf::from(output), bytes).unwrap();
}

#[derive(Debug)]
struct Converted {
    document: into_markdown_core::Document,
    assets: Vec<into_markdown_core::Asset>,
    markdown: String,
}

fn convert(fixture: &Fixture) -> Result<Converted, ConversionError> {
    convert_with_limit(fixture, None)
}

fn convert_with_limit(
    fixture: &Fixture,
    limit: Option<(&str, u64)>,
) -> Result<Converted, ConversionError> {
    let bytes = std::fs::read(fixture_root().join(&fixture.path))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != fixture.bytes
        || sha256(&bytes) != fixture.sha256
    {
        return Err(ConversionError::Malformed {
            part: Some(fixture.path.clone()),
            detail: "fixture bytes do not match the corpus manifest".into(),
        });
    }
    let format = input_format(&fixture.format);
    let mut options = ConversionOptions::default();
    if let Some((name, value)) = limit {
        match name {
            "max_input_bytes" => options.limits.max_input_bytes = value,
            "max_memory_bytes" => options.limits.max_memory_bytes = value,
            "max_nesting_depth" => {
                options.limits.max_nesting_depth =
                    u16::try_from(value).map_err(|_| ConversionError::ResourceLimit {
                        limit: "max_nesting_depth",
                        detail: "fixture limit cannot be represented as u16".into(),
                    })?;
            }
            "max_table_columns" => options.limits.max_table_columns = value,
            "max_table_rows" => options.limits.max_table_rows = value,
            other => {
                return Err(ConversionError::Internal {
                    detail: format!("unsupported quality fixture limit {other}"),
                });
            }
        }
    }
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some(fixture.id.clone()),
            media_type: Some(fixture.media_type.clone()),
            uri: None,
            size: fixture.bytes,
        },
    };
    let services = Services { nested: Some(Arc::new(HtmlNested)), ..Services::default() };
    let converter = converter(format);
    let candidate = FormatCandidate::explicit(format);
    let output = if format == InputFormat::Xlsx {
        let planned = converter.planned_output_bytes(&input, &candidate, &options, &context)?;
        let mut reservation = context.reserve_memory(planned)?;
        let credit = context.with_memory_credit(&mut reservation)?;
        block_on(converter.convert(&input, &candidate, &options, &services, &credit))?
    } else {
        block_on(converter.convert(&input, &candidate, &options, &services, &context))?
    };
    let markdown =
        into_markdown_render_markdown::render(&output.document, &output.assets, &options)?;
    Ok(Converted { document: output.document, assets: output.assets, markdown })
}

fn converter(format: InputFormat) -> Box<dyn Converter> {
    match format {
        InputFormat::Docx => Box::new(DocxConverter),
        InputFormat::Pptx => Box::new(PresentationConverter),
        InputFormat::Xlsx => Box::new(WorkbookConverter),
        InputFormat::Odt | InputFormat::Ods | InputFormat::Odp => Box::new(OdfConverter),
        InputFormat::Rtf => Box::new(RtfConverter),
        InputFormat::Epub => Box::new(EpubConverter),
        InputFormat::OutlookMsg => Box::new(MsgConverter),
        other => panic!("unsupported semantic layout fixture format {other:?}"),
    }
}

fn input_format(format: &str) -> InputFormat {
    match format {
        "docx" => InputFormat::Docx,
        "pptx" => InputFormat::Pptx,
        "xlsx" => InputFormat::Xlsx,
        "odt" => InputFormat::Odt,
        "ods" => InputFormat::Ods,
        "odp" => InputFormat::Odp,
        "rtf" => InputFormat::Rtf,
        "epub" => InputFormat::Epub,
        "outlook-msg" => InputFormat::OutlookMsg,
        other => panic!("unknown semantic layout fixture format {other}"),
    }
}

struct HtmlNested;

impl NestedConversionService for HtmlNested {
    fn convert<'a>(
        &'a self,
        request: NestedConversionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            HtmlConverter
                .convert(
                    request.input,
                    &FormatCandidate::explicit(InputFormat::Html),
                    request.options,
                    &Services::default(),
                    context,
                )
                .await
        })
    }
}

fn fixture<'a>(manifest: &'a Manifest, id: &str) -> &'a Fixture {
    manifest.fixtures.iter().find(|fixture| fixture.id == id).unwrap_or_else(|| panic!("{id}"))
}

fn manifest() -> (Vec<u8>, Manifest) {
    let bytes = std::fs::read(fixture_root().join("manifest.json")).unwrap();
    let manifest = serde_json::from_slice(&bytes).unwrap();
    (bytes, manifest)
}

fn authority_bundle() -> AuthorityBundle {
    serde_json::from_slice(
        &std::fs::read(fixture_root().join("semantic-layout-quality-authority.json")).unwrap(),
    )
    .unwrap()
}

fn fixture_root() -> PathBuf {
    std::env::var_os("TEST_SRCDIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"),
        |runfiles| {
            PathBuf::from(runfiles)
                .join(std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into()))
                .join("fixtures")
        },
    )
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

const FAMILY_COVERAGE: [(&str, &str, &str, &str, &str, &str); 12] = [
    (
        "wordprocessingml-docx-docm",
        "docx-normal",
        "docx-malicious",
        "docx-corrupt",
        "docx-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "presentationml",
        "pptx-normal",
        "pptm-malicious",
        "pptx-corrupt",
        "pptx-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "spreadsheetml-xlsb",
        "xlsx-normal",
        "xlsb-normal",
        "xlsb-corrupt",
        "xlsb-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "odf-text",
        "odt-normal",
        "odt-implicit-nested-list",
        "odt-corrupt",
        "odt-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "odf-presentation",
        "odp-normal",
        "odp-rotation",
        "odp-corrupt",
        "odp-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "odf-spreadsheet",
        "ods-normal",
        "ods-span-nested",
        "ods-corrupt",
        "ods-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "rtf",
        "rtf-normal",
        "rtf-malicious",
        "rtf-corrupt",
        "rtf-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "epub",
        "epub-normal",
        "gate:epub-container-tests",
        "gate:epub-malformed-container-tests",
        "gate:epub-resource-limits",
        "//crates/converters:converters_test",
    ),
    (
        "pdf",
        "pdf-layout-multicolumn",
        "pdf-layout-structures",
        "gate:pdf-envelope-tests",
        "gate:pdf-layout-resource-limits",
        "//crates/converters:pdf_layout_quality",
    ),
    (
        "image-ocr",
        "ocr-english-clear-1",
        "ocr-mixed-clear-1",
        "gate:image-envelope-tests",
        "gate:image-resource-limits",
        "//crates/api:ppocrv6_image_quality",
    ),
    (
        "outlook-msg",
        "msg-normal",
        "msg-attachment-nested",
        "msg-corrupt",
        "msg-limit",
        "//crates/layout-quality:semantic_layout_quality",
    ),
    (
        "legacy-office",
        "gate:legacy-office-installed-smoke",
        "gate:legacy-office-complex-smoke",
        "gate:legacy-office-corrupt-input",
        "gate:legacy-office-resource-limits",
        "//crates/legacy-office:legacy_office_test",
    ),
];

fn coverage() -> Vec<FamilyCoverage> {
    FAMILY_COVERAGE
        .into_iter()
        .map(|(format_family, normal, complex, corrupt, boundary, gate)| FamilyCoverage {
            format_family: format_family.into(),
            normal_fixture: normal.into(),
            complex_fixture: complex.into(),
            misordered_counterexample: "counterexample:reading-order".into(),
            corrupt_fixture: corrupt.into(),
            resource_boundary_fixture: boundary.into(),
            gate: gate.into(),
        })
        .collect()
}
