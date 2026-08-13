use sha2::{Digest, Sha256};

#[test]
fn degraded_scan_quality_authority_is_hash_and_license_bound() {
    let authority: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../models/ocr-merge-quality-authority.json"
    ))
    .unwrap();
    assert_eq!(authority["schema_version"], 1);
    assert_eq!(authority["fixture_license"], "Apache-2.0");
    assert_eq!(authority["fixture_provenance"], "repository-generated");
    assert_eq!(authority["normalization"], "unicode-nfc-remove-unicode-whitespace");
    assert_eq!(authority["maximum_aggregate_cer"], 0.15);
    assert_eq!(authority["expected_evaluated_characters"], 431);
    assert_eq!(
        authority["degradation"]["algorithm"],
        "contrast-compress-and-sparse-background-speckle"
    );
    assert_eq!(authority["degradation"]["seed"], 20_260_813);
    assert_eq!(authority["degradation"]["black_level"], 24);
    assert_eq!(authority["degradation"]["white_level"], 238);
    assert_eq!(authority["degradation"]["speckle_modulus"], 997);
    assert_eq!(authority["degradation"]["speckle_remainder"], 0);
    assert_eq!(authority["degradation"]["speckle_source_minimum"], 224);
    assert_eq!(authority["degradation"]["speckle_level"], 176);

    let manifest = include_bytes!("../../../../../fixtures/manifest.json");
    let recognizer = include_bytes!("../../../../../models/ppocrv6-tiny-recognizer-authority.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(manifest)),
        authority["fixture_manifest_sha256"].as_str().unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(recognizer)),
        authority["recognizer_authority_sha256"].as_str().unwrap()
    );
}
