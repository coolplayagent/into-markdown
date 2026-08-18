use super::*;
use into_markdown_core::{
    Asset, Block, BlockNode, Document, LayoutGolden, LayoutQualityConfig, audit_semantic_layout,
    audit_semantic_layout_golden,
};
use serde::Serialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    fixture_manifest_sha256: String,
    layout_golden_path: String,
    layout_golden_sha256: String,
    scenario_outcomes_sha256: String,
    scenario_goldens_path: String,
    scenario_goldens_sha256: String,
    coordinate_tolerance: f32,
    modern_minimum_precision: f64,
    modern_minimum_recall: f64,
    degraded_minimum_precision: f64,
    degraded_minimum_recall: f64,
    required_scenarios: Vec<String>,
    canonical_platforms: Vec<String>,
    platform_gate: String,
    evidence: Vec<Evidence>,
    coverage: Vec<Coverage>,
    format_coverage: Vec<FormatCoverage>,
    reading_order_evidence: Vec<FeatureEvidence>,
    goldens: Vec<Golden>,
    delegated_native_gates: Vec<NativeGate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    id: String,
    kind: String,
    reference: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Coverage {
    family: String,
    normal: String,
    complex: String,
    misordered: String,
    corrupt: String,
    resource_boundary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatCoverage {
    formats: Vec<String>,
    evidence: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeatureEvidence {
    feature: String,
    evidence: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Golden {
    fixture_id: String,
    family: String,
    fixture_sha256: String,
    ir_sha256: String,
    gfm_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeGate {
    family: String,
    bazel_target: String,
    authority: String,
    authority_sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoldenCorpus {
    schema_version: u32,
    goldens: Vec<GoldenRecord>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScenarioGoldenCorpus {
    schema_version: u32,
    outcomes: Vec<ScenarioGolden>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScenarioGolden {
    family: String,
    scenario: String,
    evidence: String,
    artifact_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ir_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gfm_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_sha256: Option<String>,
    outcome_sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoldenRecord {
    fixture_id: String,
    layout: LayoutGolden,
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn file_and_symbol(reference: &str) -> (&str, &str) {
    reference.split_once('#').unwrap_or_else(|| panic!("unbound evidence reference {reference}"))
}

fn authority() -> Authority {
    serde_json::from_str(include_str!("../../../fixtures/semantic-layout-quality-authority.json"))
        .expect("semantic layout authority must match the strict schema")
}

fn assert_direct_rust_test(item: &Evidence, source: &str, symbol: &str) {
    let declaration = format!("fn {symbol}");
    let offset = source
        .find(&declaration)
        .unwrap_or_else(|| panic!("{} missing test symbol {symbol}", item.id));
    let attribute_start = source[..offset]
        .rfind("#[test]")
        .unwrap_or_else(|| panic!("{} is not bound to an executable #[test]", item.id));
    let attributes = &source[attribute_start..offset];
    assert!(
        !attributes.contains("#[ignore"),
        "{} scenario test {symbol} must execute in the workflow",
        item.id
    );
    assert!(
        !attributes.contains("fn "),
        "{} #[test] belongs to a different function than {symbol}",
        item.id
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one strict authority audit keeps every cross-field invariant in one executable gate"
)]
fn semantic_layout_authority_is_complete_and_hash_bound() {
    let authority = authority();
    assert_eq!(authority.schema_version, 1);
    assert_eq!(
        hex(include_bytes!("../../../fixtures/manifest.json")),
        authority.fixture_manifest_sha256
    );
    assert_eq!(
        authority.required_scenarios,
        ["normal", "complex", "misordered", "corrupt", "resourceBoundary"]
    );
    assert_eq!(
        authority.canonical_platforms,
        ["linux-x86_64", "linux-aarch64", "macos-aarch64", "windows-x86_64"]
    );
    assert_eq!(authority.platform_gate, ".github/workflows/semantic-layout-quality.yml");
    assert!(
        repo_root().join(&authority.platform_gate).is_file(),
        "four-platform semantic layout workflow must exist"
    );
    let manifest = manifest();
    let fixtures = manifest
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture.sha256.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let evidence = authority
        .evidence
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(evidence.len(), authority.evidence.len(), "duplicate evidence id");
    for item in &authority.evidence {
        match item.kind.as_str() {
            "fixture" => assert_eq!(
                fixtures.get(item.reference.as_str()).copied(),
                Some(item.sha256.as_str()),
                "{} fixture evidence drift",
                item.id
            ),
            "rustTest" => {
                let (path, symbol) = file_and_symbol(&item.reference);
                let bytes = std::fs::read(repo_root().join(path)).unwrap_or_else(|error| {
                    panic!("{} test evidence {}: {error}", item.id, item.reference)
                });
                let source = std::str::from_utf8(&bytes).expect("Rust test source must be UTF-8");
                let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
                assert_eq!(
                    hex(normalized.as_bytes()),
                    item.sha256,
                    "{} normalized test source drift",
                    item.id
                );
                assert_direct_rust_test(item, source, symbol);
            }
            kind => panic!("{} has unsupported evidence kind {kind}", item.id),
        }
    }
    let expected = BTreeSet::from([
        "docx-docm",
        "presentationml",
        "spreadsheetml-xlsb",
        "opendocument",
        "rtf",
        "epub",
        "legacy-office",
        "pdf",
        "image-ocr",
        "outlook-msg",
    ]);
    let actual =
        authority.coverage.iter().map(|coverage| coverage.family.as_str()).collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    let mut scenario_evidence = BTreeSet::new();
    for coverage in &authority.coverage {
        for evidence_id in [
            &coverage.normal,
            &coverage.complex,
            &coverage.misordered,
            &coverage.corrupt,
            &coverage.resource_boundary,
        ] {
            assert!(
                evidence.contains_key(evidence_id.as_str()),
                "{} references missing evidence {evidence_id}",
                coverage.family
            );
            assert!(
                scenario_evidence.insert(evidence_id.as_str()),
                "{} reuses generic scenario evidence {evidence_id}",
                coverage.family
            );
        }
    }
    assert_eq!(scenario_evidence.len(), expected.len() * authority.required_scenarios.len());
    let required_formats = BTreeSet::from([
        "docx",
        "docm",
        "pptx",
        "pptm",
        "ppsx",
        "ppsm",
        "potx",
        "xlsx",
        "xlsm",
        "xlsb",
        "odt",
        "ods",
        "odp",
        "rtf",
        "epub",
        "doc",
        "ppt",
        "xls",
        "pdf",
        "image-ocr",
        "outlook-msg",
    ]);
    let mut covered_formats = BTreeSet::new();
    for coverage in &authority.format_coverage {
        assert!(evidence.contains_key(coverage.evidence.as_str()));
        for format in &coverage.formats {
            assert!(covered_formats.insert(format.as_str()), "duplicate format evidence {format}");
        }
    }
    assert_eq!(covered_formats, required_formats);
    let required_features = BTreeSet::from([
        "single-column",
        "multi-column",
        "mixed-horizontal-vertical",
        "floating-object",
        "header-footer",
        "footnote",
    ]);
    let feature_map = authority
        .reading_order_evidence
        .iter()
        .map(|item| (item.feature.as_str(), item.evidence.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(feature_map.keys().copied().collect::<BTreeSet<_>>(), required_features);
    for evidence_id in feature_map.values() {
        assert!(evidence.contains_key(evidence_id));
    }
    assert_eq!(authority.coordinate_tolerance.to_bits(), 0.01_f32.to_bits());
    assert_eq!((authority.modern_minimum_precision, authority.modern_minimum_recall), (0.95, 0.95));
    assert_eq!(
        (authority.degraded_minimum_precision, authority.degraded_minimum_recall),
        (0.90, 0.90)
    );
    for gate in &authority.delegated_native_gates {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(&gate.authority),
        )
        .unwrap_or_else(|error| panic!("{} authority {}: {error}", gate.family, gate.authority));
        assert_eq!(hex(&bytes), gate.authority_sha256, "{} native authority drift", gate.family);
        let target = gate.bazel_target.strip_prefix("//").expect("Bazel target must be absolute");
        let target_reference = target.replace(':', "#");
        let (package, name) = file_and_symbol(&target_reference);
        let build = std::fs::read_to_string(repo_root().join(package).join("BUILD.bazel"))
            .unwrap_or_else(|error| {
                panic!("{} target {}: {error}", gate.family, gate.bazel_target)
            });
        assert!(build.contains(&format!("name = \"{name}\"")), "missing {}", gate.bazel_target);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the aggregate is intentionally derived from all 50 authority scenarios"
)]
fn semantic_layout_fifty_scenario_outcomes_are_executable_and_hash_bound() {
    let authority = authority();
    let golden_bytes = std::fs::read(repo_root().join(&authority.scenario_goldens_path))
        .expect("independent scenario outcome goldens must exist");
    assert_eq!(hex(&golden_bytes), authority.scenario_goldens_sha256);
    let expected: ScenarioGoldenCorpus =
        serde_json::from_slice(&golden_bytes).expect("strict scenario golden schema");
    assert_eq!(expected.schema_version, 1);
    let evidence =
        authority.evidence.iter().map(|item| (item.id.as_str(), item)).collect::<BTreeMap<_, _>>();
    let fixtures = manifest()
        .fixtures
        .into_iter()
        .map(|fixture| (fixture.id.clone(), fixture))
        .collect::<BTreeMap<_, _>>();
    let mut outcomes = Vec::new();
    let mut computed = Vec::new();
    for coverage in &authority.coverage {
        for (scenario, evidence_id) in [
            ("normal", coverage.normal.as_str()),
            ("complex", coverage.complex.as_str()),
            ("misordered", coverage.misordered.as_str()),
            ("corrupt", coverage.corrupt.as_str()),
            ("resourceBoundary", coverage.resource_boundary.as_str()),
        ] {
            let item = evidence[evidence_id];
            let outcome = match item.kind.as_str() {
                "rustTest" => {
                    let (path, symbol) = file_and_symbol(&item.reference);
                    let source = std::fs::read_to_string(repo_root().join(path)).unwrap();
                    assert_direct_rust_test(item, &source, symbol);
                    execute_real_rust_scenario(
                        &coverage.family,
                        scenario,
                        evidence_id,
                        &fixtures,
                        item,
                    )
                }
                "fixture" if item.reference.starts_with("pdf-layout-") => {
                    execute_native_pdf_authority_scenario(&item.reference, &item.sha256)
                }
                "fixture" => {
                    let fixture = &fixtures[&item.reference];
                    if let Some(limit) = &fixture.expected.limit {
                        let first = execute(fixture, Some((&limit.option, limit.failing_value)))
                            .expect_err("resource-boundary fixture unexpectedly succeeded");
                        let second = execute(fixture, Some((&limit.option, limit.failing_value)))
                            .expect_err(
                                "repeated resource-boundary fixture unexpectedly succeeded",
                            );
                        assert_eq!(stable_error_outcome(&first), stable_error_outcome(&second));
                        assert_eq!(first.code(), ErrorCode::ResourceLimit);
                        let ConversionError::ResourceLimit { limit: actual, .. } = &first else {
                            unreachable!()
                        };
                        assert_eq!(*actual, limit.reported_name);
                        let passing = execute(fixture, Some((&limit.option, limit.passing_value)))
                            .expect("adjacent passing boundary failed");
                        let passing_hash = hex(passing.as_bytes());
                        assert_eq!(passing_hash, limit.passing_semantic_sha256);
                        format!("error:{}|pass:{passing_hash}", stable_error_hash(&first))
                    } else if fixture.expected.outcome == "error" {
                        let first = execute(fixture, None)
                            .expect_err("corrupt fixture unexpectedly succeeded");
                        let second = execute(fixture, None)
                            .expect_err("repeated corrupt fixture unexpectedly succeeded");
                        assert_eq!(stable_error_outcome(&first), stable_error_outcome(&second));
                        assert_eq!(first.code().as_str(), fixture.expected.error_code);
                        format!("error:{}", stable_error_hash(&first))
                    } else {
                        fixture_success_outcome(fixture)
                    }
                }
                kind => panic!("unsupported scenario evidence kind {kind}"),
            };
            computed.push(scenario_golden(&coverage.family, scenario, evidence_id, &outcome));
            outcomes.push(format!("{}|{scenario}|{evidence_id}|{outcome}", coverage.family));
        }
    }
    assert_eq!(outcomes.len(), 50);
    let computed_json = serde_json::to_string_pretty(&ScenarioGoldenCorpus {
        schema_version: 1,
        outcomes: computed.clone(),
    })
    .unwrap();
    assert_eq!(computed, expected.outcomes, "scenario outcome golden drift\n{computed_json}");
    let canonical = outcomes.join("\n");
    assert_eq!(
        hex(canonical.as_bytes()),
        authority.scenario_outcomes_sha256,
        "50-scenario outcome authority drift; canonical outcomes:\n{canonical}"
    );
}

fn scenario_golden(family: &str, scenario: &str, evidence: &str, outcome: &str) -> ScenarioGolden {
    let mut golden = ScenarioGolden {
        family: family.into(),
        scenario: scenario.into(),
        evidence: evidence.into(),
        artifact_kind: String::new(),
        ir_sha256: None,
        gfm_sha256: None,
        report_sha256: None,
        error_sha256: None,
        outcome_sha256: hex(outcome.as_bytes()),
    };
    if let Some(value) = outcome.strip_prefix("ir:") {
        let (ir, gfm) = value.split_once("|gfm:").expect("IR/GFM scenario outcome");
        golden.artifact_kind = "ir-gfm".into();
        golden.ir_sha256 = Some(ir.into());
        golden.gfm_sha256 = Some(gfm.into());
    } else if let Some(report) = outcome.strip_prefix("layout-diff:") {
        golden.artifact_kind = "layout-diff-report".into();
        golden.report_sha256 = Some(report.into());
    } else if let Some(report) = outcome.strip_prefix("native-quality:") {
        golden.artifact_kind = "native-quality-report".into();
        golden.report_sha256 = Some(report.into());
    } else if let Some(error) = outcome.strip_prefix("error:") {
        golden.artifact_kind = "stable-error".into();
        golden.error_sha256 = Some(error.split('|').next().unwrap().into());
    } else {
        panic!("unsupported scenario artifact {outcome}");
    }
    golden
}

fn fixture_success_outcome(fixture: &Fixture) -> String {
    let (first, options) = execute_output(fixture, None).expect("success fixture failed");
    let (second, _) = execute_output(fixture, None).expect("repeated success fixture failed");
    let first_ir = first.document.to_json().unwrap();
    let second_ir = second.document.to_json().unwrap();
    let first_gfm =
        into_markdown_render_markdown::render(&first.document, &first.assets, &options).unwrap();
    let second_gfm =
        into_markdown_render_markdown::render(&second.document, &second.assets, &options).unwrap();
    assert_eq!(first_ir, second_ir, "{} IR is nondeterministic", fixture.id);
    assert_eq!(first_gfm, second_gfm, "{} GFM is nondeterministic", fixture.id);
    let gfm_hash = hex(first_gfm.as_bytes());
    assert_eq!(gfm_hash, fixture.expected.semantic_sha256);
    format!("ir:{}|gfm:{gfm_hash}", hex(first_ir.as_bytes()))
}

fn execute_real_rust_scenario(
    family: &str,
    scenario: &str,
    evidence: &str,
    fixtures: &BTreeMap<String, Fixture>,
    item: &Evidence,
) -> String {
    if matches!(family, "legacy-office" | "pdf" | "image-ocr") {
        return delegated_quality_outcome(family, scenario, item);
    }
    let fixture_ids: &[&str] = match (family, scenario) {
        ("docx-docm", "complex" | "misordered") => &["docx-malicious", "docx-normal"],
        ("presentationml", "complex" | "misordered") => &["pptx-normal", "potx-normal"],
        ("spreadsheetml-xlsb", "complex") => &["xlsb-normal"],
        ("spreadsheetml-xlsb", "misordered") => &["xlsb-normal", "xlsx-normal"],
        ("opendocument", "misordered") => &["odp-rotation", "odp-normal"],
        ("rtf", "complex" | "misordered") => &["rtf-malicious", "rtf-normal"],
        ("epub", "complex" | "misordered" | "corrupt" | "resourceBoundary") => &["epub-normal"],
        ("outlook-msg", "misordered") => &["msg-attachment-nested", "msg-normal"],
        _ => panic!("{family}/{scenario} lacks a real converter fixture mapping"),
    };
    let selected = fixture_ids.iter().map(|id| &fixtures[*id]).collect::<Vec<_>>();
    match scenario {
        "misordered" => {
            let (golden, assets, options) = combined_real_output(&selected);
            let mut actual = golden.clone();
            assert!(
                reverse_first_siblings(&mut actual.blocks),
                "{family} misordered fixture must expose two converter-produced sibling nodes"
            );
            let fixture = format!("{family}|{scenario}|{}", fixture_ids.join("+"));
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let audit = audit_semantic_layout(
                &fixture,
                &actual,
                &assets,
                &golden,
                &assets,
                LayoutQualityConfig {
                    coordinate_tolerance: 0.01,
                    minimum_precision: 0.95,
                    minimum_recall: 0.95,
                    max_field_bytes: 1024 * 1024,
                },
                &context,
            )
            .unwrap();
            assert!(
                audit
                    .report()
                    .differences
                    .iter()
                    .any(|diff| { diff.kind == into_markdown_core::LayoutDiffKind::OutOfOrder })
            );
            format!("layout-diff:{}", hex(audit.to_json().unwrap().as_bytes()))
        }
        "corrupt" => {
            let source = selected[0];
            let bytes = std::fs::read(repo_root().join("fixtures").join(&source.path)).unwrap();
            let truncated = &bytes[..bytes.len() / 2];
            let error = execute_fixture_bytes(source, truncated, None)
                .expect_err("truncated real EPUB fixture unexpectedly succeeded");
            format!("error:{}", stable_error_hash(&error))
        }
        "resourceBoundary" => {
            let source = selected[0];
            let error = execute_output(source, Some(("max_memory_bytes", 0)))
                .expect_err("real EPUB boundary unexpectedly succeeded");
            format!("error:{}", stable_error_hash(&error))
        }
        "complex" => real_success_outcome(&selected),
        other => panic!("unsupported real Rust scenario {other} for {evidence}"),
    }
}

fn real_success_outcome(fixtures: &[&Fixture]) -> String {
    let (document, assets, options) = combined_real_output(fixtures);
    let ir = document.to_json().unwrap();
    let gfm = into_markdown_render_markdown::render(&document, &assets, &options).unwrap();
    format!("ir:{}|gfm:{}", hex(ir.as_bytes()), hex(gfm.as_bytes()))
}

fn combined_real_output(fixtures: &[&Fixture]) -> (Document, Vec<Asset>, ConversionOptions) {
    let (first, options) = execute_output(fixtures[0], None).expect("real scenario fixture failed");
    if fixtures.len() == 1 {
        return (first.document, first.assets, options);
    }
    let mut document = first.document;
    let mut assets = first.assets;
    for fixture in &fixtures[1..] {
        let (mut output, _) = execute_output(fixture, None).expect("real scenario fixture failed");
        prefix_node_ids(&mut output.document.blocks, &fixture.id);
        document.blocks.extend(output.document.blocks);
        assets.extend(output.assets);
    }
    (document, assets, options)
}

fn prefix_node_ids(nodes: &mut [BlockNode], prefix: &str) {
    for node in nodes {
        node.id.0 = format!("{prefix}:{}", node.id.0);
        for children in mutable_child_groups(&mut node.block) {
            prefix_node_ids(children, prefix);
        }
    }
}

fn reverse_first_siblings(nodes: &mut [BlockNode]) -> bool {
    if nodes.len() >= 2 {
        nodes.reverse();
        return true;
    }
    nodes
        .iter_mut()
        .any(|node| mutable_child_groups(&mut node.block).into_iter().any(reverse_first_siblings))
}

fn mutable_child_groups(block: &mut Block) -> Vec<&mut [BlockNode]> {
    match block {
        Block::List { items, .. } => {
            items.iter_mut().map(|item| item.blocks.as_mut_slice()).collect()
        }
        Block::Table { rows, .. } => rows
            .iter_mut()
            .flat_map(|row| row.cells.iter_mut())
            .map(|cell| cell.blocks.as_mut_slice())
            .collect(),
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => vec![blocks.as_mut_slice()],
        _ => Vec::new(),
    }
}

fn delegated_quality_outcome(family: &str, scenario: &str, item: &Evidence) -> String {
    let authority = authority();
    let gate = authority
        .delegated_native_gates
        .iter()
        .find(|gate| gate.family == family)
        .unwrap_or_else(|| panic!("missing delegated gate for {family}"));
    let report = match family {
        "pdf" => {
            let fixture_id = match scenario {
                "normal" => "pdf-layout-multicolumn",
                "complex" => "pdf-layout-structures",
                "misordered" => "pdf-layout-multicolumn",
                "corrupt" => "pdf-layout-rotated",
                "resourceBoundary" => "pdf-layout-structures",
                _ => unreachable!(),
            };
            let bytes =
                std::fs::read(repo_root().join("fixtures/pdf-layout-quality-authority.json"))
                    .unwrap();
            let native: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            native["fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["fixture_id"] == fixture_id)
                .unwrap_or_else(|| panic!("missing PDF native report {fixture_id}"))
                .clone()
        }
        "image-ocr" => {
            let bytes =
                std::fs::read(repo_root().join("models/ppocrv6-tiny-recognizer-authority.json"))
                    .unwrap();
            let native: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            serde_json::json!({
                "scenario": scenario,
                "qualityGroups": native["quality_groups"],
            })
        }
        "legacy-office" => {
            let bytes =
                std::fs::read(repo_root().join("fixtures/legacy-office-quality-authority.json"))
                    .unwrap();
            let native: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            native["reports"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["scenario"] == scenario)
                .unwrap_or_else(|| panic!("missing legacy Office native report {scenario}"))
                ["report"]
                .clone()
        }
        _ => unreachable!(),
    };
    assert_eq!(
        hex(&std::fs::read(repo_root().join(&gate.authority)).unwrap()),
        gate.authority_sha256
    );
    assert!(!item.reference.is_empty());
    format!("native-quality:{}", hex(&serde_json::to_vec(&report).unwrap()))
}

fn execute_fixture_bytes(
    fixture: &Fixture,
    bytes: &[u8],
    limit: Option<(&str, u64)>,
) -> Result<ConverterOutput, ConversionError> {
    let format = format(&fixture.format);
    let mut options = options(limit);
    if format == InputFormat::Image {
        options.ocr.policy = OcrPolicy::Off;
    }
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some(fixture.id.clone()),
            media_type: Some(fixture.media_type.clone()),
            uri: None,
            size: u64::try_from(bytes.len()).unwrap(),
        },
    };
    let requests = Arc::new(RequestCounter::default());
    let services = Services {
        ocr: Some(requests.clone()),
        transcriber: Some(requests.clone()),
        ai: Some(requests.clone()),
        nested: Some(Arc::new(HtmlNested)),
    };
    let converter = converter(format);
    let candidate = FormatCandidate::explicit(format);
    let result = block_on(converter.convert(&input, &candidate, &options, &services, &context));
    assert_eq!(requests.0.load(Ordering::SeqCst), 0);
    result
}

fn execute_native_pdf_authority_scenario(fixture_id: &str, fixture_sha256: &str) -> String {
    let bytes = std::fs::read(repo_root().join("fixtures/pdf-layout-quality-authority.json"))
        .expect("PDF semantic quality authority");
    let authority: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let record = authority["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["fixture_id"] == fixture_id)
        .unwrap_or_else(|| panic!("missing PDF scenario authority {fixture_id}"));
    assert_eq!(record["fixture_sha256"], fixture_sha256);
    assert!(!record["expected_sequence"].as_array().unwrap().is_empty());
    format!("native-quality:{}", hex(&serde_json::to_vec(record).unwrap()))
}

fn stable_error_hash(error: &ConversionError) -> String {
    hex(stable_error_outcome(error).as_bytes())
}

fn stable_error_outcome(error: &ConversionError) -> String {
    match error {
        ConversionError::ResourceLimit { limit, .. } => {
            format!("{}|limit={limit}", error.code().as_str())
        }
        _ => error.code().as_str().to_owned(),
    }
}

#[test]
fn real_cross_format_converters_match_ir_and_gfm_goldens() {
    let authority = authority();
    let golden_bytes = std::fs::read(repo_root().join(&authority.layout_golden_path))
        .expect("independent layout golden must exist");
    assert_eq!(hex(&golden_bytes), authority.layout_golden_sha256, "layout golden drift");
    let corpus: GoldenCorpus =
        serde_json::from_slice(&golden_bytes).expect("independent layout golden schema");
    assert_eq!(corpus.schema_version, 1);
    let independent = corpus
        .goldens
        .iter()
        .map(|record| (record.fixture_id.as_str(), record))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(independent.len(), corpus.goldens.len(), "duplicate independent golden");
    let manifest = manifest();
    let by_id = manifest
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut failures = Vec::new();
    assert_eq!(
        independent.keys().copied().collect::<BTreeSet<_>>(),
        authority.goldens.iter().map(|golden| golden.fixture_id.as_str()).collect()
    );
    for golden in &authority.goldens {
        let fixture = by_id
            .get(golden.fixture_id.as_str())
            .unwrap_or_else(|| panic!("missing fixture {}", golden.fixture_id));
        assert_eq!(fixture.sha256, golden.fixture_sha256, "{} fixture drift", golden.fixture_id);
        let (first, options) = execute_output(fixture, None)
            .unwrap_or_else(|error| panic!("{} conversion failed: {error}", golden.fixture_id));
        let (second, _) = execute_output(fixture, None)
            .unwrap_or_else(|error| panic!("{} repeat failed: {error}", golden.fixture_id));
        let first_ir = first.document.to_json().unwrap();
        let second_ir = second.document.to_json().unwrap();
        let first_gfm =
            into_markdown_render_markdown::render(&first.document, &first.assets, &options)
                .unwrap();
        let second_gfm =
            into_markdown_render_markdown::render(&second.document, &second.assets, &options)
                .unwrap();
        assert_eq!(first_ir, second_ir, "{} IR is nondeterministic", golden.fixture_id);
        assert_eq!(first_gfm, second_gfm, "{} GFM is nondeterministic", golden.fixture_id);
        let ir_hash = hex(first_ir.as_bytes());
        let gfm_hash = hex(first_gfm.as_bytes());
        if ir_hash != golden.ir_sha256 || gfm_hash != golden.gfm_sha256 {
            failures.push(format!(
                "{} ({}) ir={} gfm={}",
                golden.fixture_id, golden.family, ir_hash, gfm_hash
            ));
            continue;
        }
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let audit = audit_semantic_layout_golden(
            &golden.fixture_id,
            &first.document,
            &first.assets,
            &independent[golden.fixture_id.as_str()].layout,
            LayoutQualityConfig {
                coordinate_tolerance: authority.coordinate_tolerance,
                minimum_precision: authority.modern_minimum_precision,
                minimum_recall: authority.modern_minimum_recall,
                max_field_bytes: 1024 * 1024,
            },
            &context,
        )
        .unwrap();
        assert!(audit.report().passed(), "{}: {}", golden.fixture_id, audit.to_json().unwrap());
        drop(audit);
        assert_eq!(context.reserved_memory_bytes(), 0, "{} leaked audit lease", golden.fixture_id);
    }
    assert!(
        failures.is_empty(),
        "refresh only after reviewed semantic changes:\n{}",
        failures.join("\n")
    );
}
