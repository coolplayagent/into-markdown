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

    let manifest: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    let recognizer: serde_json::Value = serde_json::from_slice(recognizer).unwrap();
    let goldens = manifest["ocr_quality"]["goldens"].as_array().unwrap();
    let groups = recognizer["quality_groups"].as_array().unwrap();
    assert_eq!(goldens.len(), 12);
    assert_eq!(groups.len(), 4);
    assert_eq!(
        goldens.iter().map(|golden| golden["evaluated_characters"].as_u64().unwrap()).sum::<u64>(),
        authority["expected_evaluated_characters"].as_u64().unwrap()
    );
    for golden in goldens {
        let group = groups
            .iter()
            .find(|group| group["group"] == golden["group"])
            .expect("every OCR golden remains bound to an authorized quality group");
        assert_eq!(golden["maximum_cer"], group["maximum_cer"]);
    }
    for group in groups {
        let evaluated = goldens
            .iter()
            .filter(|golden| golden["group"] == group["group"])
            .map(|golden| golden["evaluated_characters"].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(evaluated, group["evaluated_characters"].as_u64().unwrap());
    }
}

#[test]
fn quality_authority_rejects_even_non_ocr_manifest_drift() {
    let authority: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../models/ocr-merge-quality-authority.json"
    ))
    .unwrap();
    let mut mutated = include_bytes!("../../../../../fixtures/manifest.json").to_vec();
    let offset = mutated
        .windows(b"\"xls\"".len())
        .position(|window| window == b"\"xls\"")
        .expect("the legacy XLS format is present in the authoritative manifest");
    mutated[offset + 3] = b't';
    assert_ne!(
        format!("{:x}", Sha256::digest(mutated)),
        authority["fixture_manifest_sha256"].as_str().unwrap()
    );
}
