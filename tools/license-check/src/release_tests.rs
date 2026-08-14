use crate::release::{generate_release_inputs, verify_archive_projection};
use crate::schema::{ArchiveFile, ArchiveFileKind, ArchiveProjection, FfmpegEvidence};
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
    ArchiveProjection {
        schema_version: 1,
        target: target.into(),
        components: vec![],
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
                embedded_components: vec![],
            },
        ],
        ffmpeg_evidence: None,
    }
}

fn select(projection: &mut ArchiveProjection, components: &[&str]) {
    projection.components = components.iter().map(|id| (*id).to_owned()).collect();
    let inputs =
        generate_release_inputs(&root(), &request(&projection.target, components)).unwrap();
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
    assert!(errors.iter().any(|error| error.contains("cannot be classified as project-owned")));
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

    projection.ffmpeg_evidence = Some(FfmpegEvidence {
        schema_version: 1,
        ffmpeg_version: "8.1.2".into(),
        target: projection.target.clone(),
        executable_path: "bin/ffmpeg".into(),
        executable_bytes: 99,
        executable_sha256: "c".repeat(64),
        configure: vec![
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
        .collect(),
        dependencies: vec![
            "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
            "/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia",
            "/System/Library/Frameworks/CoreVideo.framework/Versions/A/CoreVideo",
            "/usr/lib/libSystem.B.dylib",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    });
    let incompatible =
        verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
            .unwrap_err();
    assert!(incompatible.iter().any(|error| error.contains("incompatible or external")));
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
    projection
        .files
        .iter_mut()
        .find(|file| file.path == "bin/into-md")
        .unwrap()
        .embedded_components = components.iter().map(|id| (*id).to_owned()).collect();
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();

    projection.files[4].embedded_components.push("cargo:unknown@9.9.9".into());
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("embeds unknown or unselected")));
}
