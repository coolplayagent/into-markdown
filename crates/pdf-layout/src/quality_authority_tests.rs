use sha2::{Digest, Sha256};

#[test]
fn layout_quality_authority_is_hash_license_and_pipeline_bound() {
    let authority: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/pdf-layout-quality-authority.json"))
            .unwrap();
    let manifest = include_bytes!("../../../fixtures/manifest.json");
    let pdfium = include_bytes!("../../../third_party/pdfium/manifest.json");
    let ocr_merge = include_bytes!("../../../models/ocr-merge-quality-authority.json");
    assert_eq!(authority["schema_version"], 1);
    assert_eq!(authority["fixture_license"], "Apache-2.0");
    assert_eq!(authority["fixture_provenance"], "repository-generated");
    assert_eq!(authority["minimum_semantic_precision"], 0.90);
    assert_eq!(authority["minimum_semantic_recall"], 0.90);
    assert_eq!(authority["fixture_manifest_sha256"], hex(manifest));
    assert_eq!(authority["pdfium_manifest_sha256"], hex(pdfium));
    assert_eq!(authority["ocr_merge_authority_sha256"], hex(ocr_merge));
    assert_eq!(authority["fixtures"].as_array().unwrap().len(), 5);
    let manifest: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    for fixture in authority["fixtures"].as_array().unwrap() {
        let id = fixture["fixture_id"].as_str().unwrap();
        let record = manifest["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == id)
            .unwrap();
        assert_eq!(record["format"], "pdf");
        assert_eq!(record["license"]["spdx"], "Apache-2.0");
        assert_eq!(record["provenance"]["kind"], "repository-generated");
        assert_eq!(fixture["fixture_sha256"], record["sha256"]);
        assert!(!fixture["expected_sequence"].as_array().unwrap().is_empty());
    }
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
