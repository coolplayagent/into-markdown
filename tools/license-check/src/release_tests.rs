use crate::release::{generate_release_inputs, verify_archive_projection};
use crate::schema::{
    ArchiveFile, ArchiveFileKind, ArchiveProjection, FfmpegEvidence, LicenseMaterial,
    LicenseMaterialKind,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    crate::repository_root().unwrap()
}

fn request(target: &str, components: &[&str]) -> String {
    serde_json::json!({
        "schema_version": 1,
        "target": target,
        "components": components,
    })
    .to_string()
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn declaration(path: &str, bytes: u64, sha256: String, kind: ArchiveFileKind) -> ArchiveFile {
    ArchiveFile {
        path: path.into(),
        bytes,
        sha256,
        kind,
        component_id: None,
        embedded_components: vec![],
    }
}

fn minimal_projection(target: &str) -> ArchiveProjection {
    let repository = root();
    let inputs = generate_release_inputs(&repository, &request(target, &[])).unwrap();
    let license = fs::read(repository.join("LICENSE")).unwrap();
    let mut projection = ArchiveProjection {
        schema_version: 1,
        target: target.into(),
        components: inputs.component_ids.clone(),
        files: vec![
            declaration(
                "LICENSE",
                license.len() as u64,
                hash(&license),
                ArchiveFileKind::Declaration,
            ),
            declaration(
                "NOTICE",
                inputs.notice.bytes,
                inputs.notice.sha256.clone(),
                ArchiveFileKind::Declaration,
            ),
            declaration(
                "THIRD_PARTY_NOTICES.md",
                inputs.third_party_notices.bytes,
                inputs.third_party_notices.sha256.clone(),
                ArchiveFileKind::Generated,
            ),
            declaration(
                "sbom-input.json",
                inputs.sbom_input.bytes,
                inputs.sbom_input.sha256.clone(),
                ArchiveFileKind::Generated,
            ),
            ArchiveFile {
                path: "bin/into-md".into(),
                bytes: 1,
                sha256: "a".repeat(64),
                kind: ArchiveFileKind::Project,
                component_id: None,
                embedded_components: inputs.component_ids.clone(),
            },
        ],
        license_materials: vec![],
        ffmpeg_evidence: None,
    };
    install_base_materials(&mut projection, &inputs);
    projection
}

fn select(projection: &mut ArchiveProjection, components: &[&str]) {
    let inputs =
        generate_release_inputs(&root(), &request(&projection.target, components)).unwrap();
    projection.components = inputs.component_ids.clone();
    projection.files.retain(|file| file.kind != ArchiveFileKind::LicenseMaterial);
    projection.license_materials.clear();
    install_base_materials(projection, &inputs);
    let binary =
        projection.files.iter_mut().find(|file| file.kind == ArchiveFileKind::Project).unwrap();
    binary.embedded_components = inputs
        .component_ids
        .iter()
        .filter(|id| {
            id.starts_with("cargo:")
                || id.starts_with("npm:")
                || matches!(
                    id.as_str(),
                    "imageproc-contour-adaptation" | "clipper2-rust" | "calamine"
                )
        })
        .cloned()
        .collect();
    if components.contains(&"ffmpeg") {
        let source: serde_json::Value = serde_json::from_slice(
            &fs::read(root().join("third_party/ffmpeg/source.json")).unwrap(),
        )
        .unwrap();
        push_external_material(
            projection,
            "share/into-markdown/source/ffmpeg-8.1.2.tar.xz",
            source["source_bytes"].as_u64().unwrap(),
            source["source_sha256"].as_str().unwrap(),
            LicenseMaterialKind::CorrespondingSource,
            "ffmpeg",
        );
        push_external_material(
            projection,
            "share/into-markdown/relink/ffmpeg-relink-materials.tar",
            10,
            &"9".repeat(64),
            LicenseMaterialKind::RelinkMaterial,
            "ffmpeg",
        );
    }
    if components.contains(&"pdfium") {
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(root().join("third_party/pdfium/manifest.json")).unwrap(),
        )
        .unwrap();
        let target = &manifest["targets"][&projection.target];
        push_external_material(
            projection,
            "share/into-markdown/licenses/pdfium-upstream-license-bundle.tgz",
            target["archive_size"].as_u64().unwrap(),
            target["archive_sha256"].as_str().unwrap(),
            LicenseMaterialKind::NoticeBundle,
            "pdfium",
        );
    }
    for (path, bytes, sha256) in [
        ("NOTICE", inputs.notice.bytes, inputs.notice.sha256),
        (
            "THIRD_PARTY_NOTICES.md",
            inputs.third_party_notices.bytes,
            inputs.third_party_notices.sha256,
        ),
        ("sbom-input.json", inputs.sbom_input.bytes, inputs.sbom_input.sha256),
    ] {
        let file = projection.files.iter_mut().find(|file| file.path == path).unwrap();
        file.bytes = bytes;
        file.sha256 = sha256;
    }
}

#[allow(clippy::too_many_lines)]
fn install_base_materials(
    projection: &mut ArchiveProjection,
    inputs: &crate::schema::ReleaseInputs,
) {
    let sbom: serde_json::Value = serde_json::from_str(&inputs.sbom_input.contents).unwrap();
    let mut npm = Vec::new();
    for id in &inputs.component_ids {
        if id.starts_with("cargo:") {
            let component = sbom["components"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id.as_str())
                .unwrap();
            let checksum = component["integrity"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| {
                    item["subject"].as_str().unwrap_or_default().starts_with("crates.io archive")
                })
                .unwrap()["digest"]
                .as_str()
                .unwrap();
            let safe = id.replace([':', '@', '/', '+'], "_");
            push_external_material(
                projection,
                &format!("share/into-markdown/licenses/cargo/{safe}.crate"),
                1,
                checksum,
                LicenseMaterialKind::UpstreamSourceArchive,
                id,
            );
        } else if id.starts_with("npm:") {
            npm.push(id.clone());
        }
    }
    if !npm.is_empty() {
        push_text_material(
            projection,
            "share/into-markdown/licenses/npm/react-MIT.txt",
            "third_party/licenses/npm/react-MIT.txt",
            npm,
            &["MIT"],
        );
    }
    for (id, archive_path, authority_path, license) in [
        (
            "imageproc-contour-adaptation",
            "share/into-markdown/licenses/imageproc-MIT.txt",
            "third_party/licenses/imageproc-MIT.txt",
            "MIT",
        ),
        (
            "clipper2-rust",
            "share/into-markdown/licenses/BSL-1.0.txt",
            "third_party/licenses/BSL-1.0.txt",
            "BSL-1.0",
        ),
        (
            "calamine",
            "share/into-markdown/licenses/calamine-MIT.txt",
            "third_party/licenses/calamine-MIT.txt",
            "MIT",
        ),
    ] {
        if inputs.component_ids.iter().any(|component| component == id) {
            push_text_material(
                projection,
                archive_path,
                authority_path,
                vec![id.to_owned()],
                &[license],
            );
        }
    }
    let models: Vec<_> = inputs
        .component_ids
        .iter()
        .filter(|id| {
            matches!(
                id.as_str(),
                "ppocrv6-tiny-detector-onnx-model"
                    | "ppocrv6-tiny-recognizer-onnx-model"
                    | "ppocrv6-tiny-recognizer-character-table"
            )
        })
        .cloned()
        .collect();
    if !models.is_empty() {
        push_text_material(
            projection,
            "share/into-markdown/licenses/model-Apache-2.0.txt",
            "LICENSE",
            models,
            &["Apache-2.0"],
        );
    }
    if inputs.component_ids.iter().any(|id| id == "onnxruntime-cpu") {
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(root().join("third_party/onnxruntime/manifest.json")).unwrap(),
        )
        .unwrap();
        let authority = &manifest["targets"][&projection.target];
        push_external_material(
            projection,
            "share/into-markdown/licenses/onnxruntime-upstream-bundle.tgz",
            1,
            authority["sha256"].as_str().unwrap(),
            LicenseMaterialKind::NoticeBundle,
            "onnxruntime-cpu",
        );
    }
}

fn push_text_material(
    projection: &mut ArchiveProjection,
    archive_path: &str,
    authority_path: &str,
    component_ids: Vec<String>,
    licenses: &[&str],
) {
    let contents = fs::read_to_string(root().join(authority_path)).unwrap();
    let bytes = contents.len() as u64;
    let sha256 = hash(contents.as_bytes());
    projection.files.push(declaration(
        archive_path,
        bytes,
        sha256.clone(),
        ArchiveFileKind::LicenseMaterial,
    ));
    projection.license_materials.push(LicenseMaterial {
        path: archive_path.to_owned(),
        bytes,
        sha256,
        kind: LicenseMaterialKind::LicenseText,
        component_ids,
        spdx_expressions: licenses.iter().map(|item| (*item).to_owned()).collect(),
        contents: Some(contents),
    });
}

fn push_external_material(
    projection: &mut ArchiveProjection,
    path: &str,
    bytes: u64,
    sha256: &str,
    kind: LicenseMaterialKind,
    component: &str,
) {
    projection.files.push(declaration(
        path,
        bytes,
        sha256.to_owned(),
        ArchiveFileKind::LicenseMaterial,
    ));
    projection.license_materials.push(LicenseMaterial {
        path: path.to_owned(),
        bytes,
        sha256: sha256.to_owned(),
        kind,
        component_ids: vec![component.to_owned()],
        spdx_expressions: vec![],
        contents: None,
    });
}

#[test]
fn four_platform_requests_use_one_license_conclusion() {
    let repository = root();
    let mut notices = Vec::new();
    for target in crate::schema::SUPPORTED_TARGETS {
        let fixture =
            repository.join(format!("tools/license-check/fixtures/release-request-{target}.json"));
        let json = fs::read_to_string(fixture).unwrap();
        let generated = generate_release_inputs(&repository, &json).unwrap();
        assert_eq!(generated.target, target);
        assert!(!generated.third_party_notices.contents.contains("License: LGPL-2.1-or-later"));
        for required in ["imageproc-contour-adaptation", "clipper2-rust", "calamine"] {
            assert!(generated.component_ids.iter().any(|id| id == required));
        }
        notices.push(generated.third_party_notices.contents);
    }
    assert!(notices.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn empty_projection_contract_is_valid_on_every_platform() {
    for target in crate::schema::SUPPORTED_TARGETS {
        let projection = serde_json::to_string(&minimal_projection(target)).unwrap();
        verify_archive_projection(&root(), &projection).unwrap();
    }
}

#[test]
fn authoritative_runtime_closure_cannot_be_omitted_or_disguised() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    let omitted =
        projection.components.iter().find(|id| id.starts_with("cargo:")).cloned().unwrap();
    projection.components.retain(|id| id != &omitted);
    projection.files[4].embedded_components.retain(|id| id != &omitted);
    projection.license_materials.retain(|item| !item.component_ids.contains(&omitted));
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("authoritative runtime components")));

    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.push(ArchiveFile {
        path: "lib/disguised-cargo.dylib".into(),
        bytes: 1,
        sha256: "8".repeat(64),
        kind: ArchiveFileKind::Component,
        component_id: projection.components.iter().find(|id| id.starts_with("cargo:")).cloned(),
        embedded_components: vec![],
    });
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("cannot hide a standalone")));
}

#[test]
fn missing_or_untyped_license_material_fails_closed() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.license_materials.clear();
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("no typed declaration")));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("lacks cryptographically fixed complete license material"))
    );
}

#[test]
fn projection_rejects_unknown_or_orphaned_components_and_missing_declarations() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.components.push("missing-runtime".into());
    projection.files.remove(0);
    projection.files.push(ArchiveFile {
        path: "lib/orphan.dylib".into(),
        bytes: 5,
        sha256: "b".repeat(64),
        kind: ArchiveFileKind::Component,
        component_id: None,
        embedded_components: vec![],
    });
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("unknown projected component")));
    assert!(errors.iter().any(|error| error.contains("orphaned")));
    assert!(errors.iter().any(|error| error.contains("missing required declaration LICENSE")));
}

#[test]
fn orphan_binary_cannot_claim_project_ownership() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.push(ArchiveFile {
        path: "lib/unreviewed.dylib".into(),
        bytes: 10,
        sha256: "e".repeat(64),
        kind: ArchiveFileKind::Project,
        component_id: None,
        embedded_components: vec![],
    });
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("outside the closed path set")));
}

#[test]
#[allow(clippy::too_many_lines)]
fn ffmpeg_requires_bound_lgpl_configuration_evidence() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    let approval_errors =
        generate_release_inputs(&root(), &request("aarch64-apple-darwin", &["ffmpeg"]))
            .unwrap_err();
    assert!(approval_errors.iter().any(|error| error.contains("no repository-approved")));
    projection.components.push("ffmpeg".into());
    projection.files.push(ArchiveFile {
        path: "bin/ffmpeg".into(),
        bytes: 99,
        sha256: "c".repeat(64),
        kind: ArchiveFileKind::Component,
        component_id: Some("ffmpeg".into()),
        embedded_components: vec![],
    });
    let without = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(without.iter().any(|error| error.contains("without LGPL-compatible")));

    let configure: Vec<String> = vec![
        "--disable-everything",
        "--disable-gpl",
        "--disable-version3",
        "--disable-nonfree",
        "--disable-network",
        "--disable-autodetect",
        "--disable-shared",
        "--enable-static",
        "--enable-libx264",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let dependencies: Vec<String> = vec![
        "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
        "/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia",
        "/System/Library/Frameworks/CoreVideo.framework/Versions/A/CoreVideo",
        "/usr/lib/libSystem.B.dylib",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let authority_contents = serde_json::json!({
        "schema_version": 1,
        "ffmpeg_version": "8.1.2",
        "target": projection.target,
        "executable_bytes": 99,
        "executable_sha256": "c".repeat(64),
        "configure": configure,
        "binary_format": "mach-o",
        "binary_architecture": "aarch64",
        "dependencies": dependencies,
        "toolchain": "Apple clang fixture",
        "source_sha256": "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c",
        "source_signature_sha256": "0a0963fccd70597838073f3e31b20f4a4d8cc2b5e577472c9a5a1f22624246f8",
        "signing_key_fingerprint": "FCF986EA15E6E293A5644F10B4322F04D67658D8",
        "build_policy_sha256": hash(&fs::read(root().join("third_party/ffmpeg/build-policy.json")).unwrap()),
        "config_log_sha256": "7".repeat(64),
        "relink_bytes": 10,
        "relink_sha256": "9".repeat(64),
    })
    .to_string();
    let authority_bytes = authority_contents.len() as u64;
    let authority_sha256 = hash(authority_contents.as_bytes());
    projection.ffmpeg_evidence = Some(FfmpegEvidence {
        authority_path: "share/into-markdown/authority/ffmpeg-aarch64-apple-darwin.json".into(),
        authority_bytes,
        authority_sha256: authority_sha256.clone(),
        authority_contents,
        schema_version: 1,
        ffmpeg_version: "8.1.2".into(),
        target: projection.target.clone(),
        executable_path: "bin/ffmpeg".into(),
        executable_bytes: 99,
        executable_sha256: "c".repeat(64),
        configure,
        dependencies,
        binary_format: "mach-o".into(),
        binary_architecture: "aarch64".into(),
        toolchain: "Apple clang fixture".into(),
        source_sha256: "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c".into(),
        source_signature_sha256: "0a0963fccd70597838073f3e31b20f4a4d8cc2b5e577472c9a5a1f22624246f8"
            .into(),
        signing_key_fingerprint: "FCF986EA15E6E293A5644F10B4322F04D67658D8".into(),
        build_policy_sha256: hash(
            &fs::read(root().join("third_party/ffmpeg/build-policy.json")).unwrap(),
        ),
        config_log_sha256: "7".repeat(64),
        relink_bytes: 10,
        relink_sha256: "9".repeat(64),
    });
    projection.files.push(ArchiveFile {
        path: "share/into-markdown/authority/ffmpeg-aarch64-apple-darwin.json".into(),
        bytes: authority_bytes,
        sha256: authority_sha256,
        kind: ArchiveFileKind::Component,
        component_id: Some("ffmpeg".into()),
        embedded_components: vec![],
    });
    let incompatible =
        verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
            .unwrap_err();
    assert!(incompatible.iter().any(|error| error.contains("reviewed build policy")));
    assert!(
        incompatible.iter().any(|error| error.contains("exact corresponding source or relink"))
    );
}

#[test]
fn marker_padded_license_text_is_rejected_against_fixed_authority_bytes() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    let material = projection
        .license_materials
        .iter_mut()
        .find(|item| item.component_ids == ["imageproc-contour-adaptation"])
        .unwrap();
    let fake = format!("Permission is hereby granted\n{}", "not the license ".repeat(100));
    material.contents = Some(fake.clone());
    material.bytes = fake.len() as u64;
    material.sha256 = hash(fake.as_bytes());
    let file = projection.files.iter_mut().find(|file| file.path == material.path).unwrap();
    file.bytes = material.bytes;
    file.sha256 = material.sha256.clone();
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.contains("cryptographically fixed complete license material"))
    );
}

#[test]
fn schemas_reject_unknown_fields_and_unfixed_hashes() {
    let json =
        r#"{"schema_version":1,"target":"aarch64-apple-darwin","components":[],"extra":true}"#;
    assert!(generate_release_inputs(&root(), json).unwrap_err()[0].contains("unknown field"));

    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files[0].sha256 = "A".repeat(64);
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("lacks fixed size or SHA-256")));
}

#[test]
fn non_release_eligible_authority_is_fail_closed() {
    let errors = generate_release_inputs(
        &root(),
        &request("aarch64-apple-darwin", &["onnx-protobuf-schema"]),
    )
    .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("not release-eligible")));
}

#[test]
fn sbom_input_carries_ecosystem_and_native_integrity_authority() {
    let generated = generate_release_inputs(
        &root(),
        &request(
            "aarch64-apple-darwin",
            &["cargo:serde@1.0.229", "npm:react@19.2.8", "onnxruntime-cpu"],
        ),
    )
    .unwrap();
    let sbom = &generated.sbom_input.contents;
    assert!(sbom.contains("\"algorithm\": \"SHA-256\""));
    assert!(sbom.contains("sha512-"));
    assert!(sbom.contains("Cargo.lock + third_party/licenses/rust-lock.tsv"));
    assert!(sbom.contains("third_party/onnxruntime/manifest.json"));
    assert!(sbom.contains("c04fe65021445904a3cae047272cad05e648282c75bf1f9eb7b3440120ae13dc"));
    assert!(!sbom.contains("5715f06d8992ca8eeeddcce43df3a7d38f97d537052126f558e912cb312460ca"));
}

#[test]
fn native_runtime_is_bound_to_target_manifest_and_download() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    select(&mut projection, &["onnxruntime-cpu"]);
    projection.files.extend(ocr_model_files());
    projection.files.push(ArchiveFile {
        path: "lib/libonnxruntime.dylib".into(),
        bytes: 43_184_400,
        sha256: "c04fe65021445904a3cae047272cad05e648282c75bf1f9eb7b3440120ae13dc".into(),
        kind: ArchiveFileKind::Component,
        component_id: Some("onnxruntime-cpu".into()),
        embedded_components: vec![],
    });
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();
    projection.files.last_mut().unwrap().sha256 = "d".repeat(64);
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("target library size and SHA-256")));
}

#[test]
fn model_runtime_is_bound_to_exact_reviewed_bytes() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    select(
        &mut projection,
        &[
            "ppocrv6-tiny-detector-onnx-model",
            "ppocrv6-tiny-recognizer-onnx-model",
            "ppocrv6-tiny-recognizer-character-table",
        ],
    );
    projection.files.extend([
        ArchiveFile {
            path: "share/models/detector/inference.onnx".into(),
            bytes: 1_780_590,
            sha256: "193bab7a04fca699a6c82e6abb5b81bdb28177f0abd4062552b04908dafb19f8".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-detector-onnx-model".into()),
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "share/models/inference.onnx".into(),
            bytes: 4_462_639,
            sha256: "9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-recognizer-onnx-model".into()),
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "share/models/ppocrv6_tiny_dict.txt".into(),
            bytes: 27_156,
            sha256: "c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-recognizer-character-table".into()),
            embedded_components: vec![],
        },
    ]);
    projection.files.push(ArchiveFile {
        path: "lib/libonnxruntime.dylib".into(),
        bytes: 43_184_400,
        sha256: "c04fe65021445904a3cae047272cad05e648282c75bf1f9eb7b3440120ae13dc".into(),
        kind: ArchiveFileKind::Component,
        component_id: Some("onnxruntime-cpu".into()),
        embedded_components: vec![],
    });
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();
    let mut omitted = projection.clone();
    omitted
        .files
        .retain(|file| file.component_id.as_deref() != Some("ppocrv6-tiny-detector-onnx-model"));
    let errors =
        verify_archive_projection(&root(), &serde_json::to_string(&omitted).unwrap()).unwrap_err();
    assert!(errors.iter().any(|error| error.contains("fixed file authority")));
    projection
        .files
        .iter_mut()
        .find(|file| {
            file.component_id.as_deref() == Some("ppocrv6-tiny-recognizer-character-table")
        })
        .unwrap()
        .bytes -= 1;
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("fixed file authority")));
}

fn ocr_model_files() -> [ArchiveFile; 3] {
    [
        ArchiveFile {
            path: "share/models/detector/inference.onnx".into(),
            bytes: 1_780_590,
            sha256: "193bab7a04fca699a6c82e6abb5b81bdb28177f0abd4062552b04908dafb19f8".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-detector-onnx-model".into()),
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "share/models/recognizer/inference.onnx".into(),
            bytes: 4_462_639,
            sha256: "9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-recognizer-onnx-model".into()),
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "share/models/ppocrv6_tiny_dict.txt".into(),
            bytes: 27_156,
            sha256: "c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("ppocrv6-tiny-recognizer-character-table".into()),
            embedded_components: vec![],
        },
    ]
}

#[test]
fn caller_duplicate_ids_fail_before_required_component_augmentation() {
    let errors = generate_release_inputs(
        &root(),
        &request(
            "aarch64-apple-darwin",
            &["imageproc-contour-adaptation", "imageproc-contour-adaptation"],
        ),
    )
    .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("duplicate projected component")));

    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.components.push("imageproc-contour-adaptation".into());
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("duplicate projected component")));
}

#[test]
fn cargo_and_npm_authorities_can_bind_embedded_project_content() {
    let components = ["cargo:serde@1.0.229", "npm:react@19.2.8"];
    let mut projection = minimal_projection("aarch64-apple-darwin");
    select(&mut projection, &components);
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();

    projection.files[4].embedded_components.push("cargo:unknown@9.9.9".into());
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("embeds unknown or unselected")));
}
