use super::*;
use into_markdown_core::{LayoutQualityConfig, audit_semantic_layout};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    fixture_manifest_sha256: String,
    coordinate_tolerance: f32,
    modern_minimum_precision: f64,
    modern_minimum_recall: f64,
    degraded_minimum_precision: f64,
    degraded_minimum_recall: f64,
    required_scenarios: Vec<String>,
    coverage: Vec<Coverage>,
    goldens: Vec<Golden>,
    delegated_native_gates: Vec<NativeGate>,
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

fn authority() -> Authority {
    serde_json::from_str(include_str!("../../../fixtures/semantic-layout-quality-authority.json"))
        .expect("semantic layout authority must match the strict schema")
}

#[test]
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
    for coverage in &authority.coverage {
        for evidence in [
            &coverage.normal,
            &coverage.complex,
            &coverage.misordered,
            &coverage.corrupt,
            &coverage.resource_boundary,
        ] {
            assert!(!evidence.trim().is_empty(), "{} has empty coverage evidence", coverage.family);
        }
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
        assert!(gate.bazel_target.starts_with("//"));
    }
}

#[test]
fn real_cross_format_converters_match_ir_and_gfm_goldens() {
    let authority = authority();
    let manifest = manifest();
    let by_id = manifest
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut failures = Vec::new();
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
        let audit = audit_semantic_layout(
            &golden.fixture_id,
            &first.document,
            &first.assets,
            &second.document,
            &second.assets,
            LayoutQualityConfig {
                coordinate_tolerance: authority.coordinate_tolerance,
                minimum_precision: authority.modern_minimum_precision,
                minimum_recall: authority.modern_minimum_recall,
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
