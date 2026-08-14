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
    let license_contents = format!(
        "Apache License\nPermission is hereby granted\nRedistribution and use in source and binary forms\nBoost Software License\nPermission to use, copy, modify, and/or distribute\nGNU LESSER GENERAL PUBLIC LICENSE\nMozilla Public License\nUNICODE LICENSE\nCommunity Data License Agreement\nThis software is provided 'as-is'\nSIL OPEN FONT LICENSE\n{}",
        "complete license body fixture. ".repeat(30)
    );
    let license_material = LicenseMaterial {
        path: "share/into-markdown/licenses/third-party-license-compendium.txt".into(),
        bytes: license_contents.len() as u64,
        sha256: hash(license_contents.as_bytes()),
        kind: LicenseMaterialKind::LicenseText,
        component_ids: inputs.component_ids.clone(),
        spdx_expressions: [
            "Apache-2.0",
            "MIT",
            "BSD-3-Clause",
            "BSL-1.0",
            "ISC",
            "LGPL-2.1-or-later",
            "MPL-2.0",
            "Unicode-3.0",
            "CDLA-Permissive-2.0",
            "Zlib",
            "OFL-1.1",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        contents: Some(license_contents),
    };
    ArchiveProjection {
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
                inputs.notice.sha256,
                ArchiveFileKind::Declaration,
            ),
            declaration(
                "THIRD_PARTY_NOTICES.md",
                inputs.third_party_notices.bytes,
                inputs.third_party_notices.sha256,
                ArchiveFileKind::Generated,
            ),
            declaration(
                "sbom-input.json",
                inputs.sbom_input.bytes,
                inputs.sbom_input.sha256,
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
            declaration(
                &license_material.path,
                license_material.bytes,
                license_material.sha256.clone(),
                ArchiveFileKind::LicenseMaterial,
            ),
        ],
        license_materials: vec![license_material],
        ffmpeg_evidence: None,
    }
}

fn select(projection: &mut ArchiveProjection, components: &[&str]) {
    let inputs =
        generate_release_inputs(&root(), &request(&projection.target, components)).unwrap();
    projection.components = inputs.component_ids.clone();
    projection.license_materials[0].component_ids = inputs.component_ids.clone();
    let binary =
        projection.files.iter_mut().find(|file| file.kind == ArchiveFileKind::Project).unwrap();
    binary.embedded_components = inputs
        .component_ids
        .iter()
        .filter(|id| id.starts_with("cargo:") || id.starts_with("npm:"))
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
        assert!(generated.third_party_notices.contents.contains("License: LGPL-2.1-or-later"));
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
    projection.license_materials[0].component_ids.retain(|id| id != &omitted);
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
    assert!(errors.iter().any(|error| error.contains("lacks complete archived license text")));
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
fn ffmpeg_requires_bound_lgpl_configuration_evidence() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
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
        &["ppocrv6-tiny-recognizer-onnx-model", "ppocrv6-tiny-recognizer-character-table"],
    );
    projection.files.extend([
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
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();
    projection.files.last_mut().unwrap().bytes -= 1;
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("fixed file authority")));
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
