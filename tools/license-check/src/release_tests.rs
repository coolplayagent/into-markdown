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
            declaration(
                "core-catalog.json",
                inputs.core_catalog.bytes,
                inputs.core_catalog.sha256.clone(),
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
        native_transformations: vec![],
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
                    "opencc-transcript-character-table"
                        | "imageproc-contour-adaptation"
                        | "clipper2-rust"
                        | "calamine"
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
        ("core-catalog.json", inputs.core_catalog.bytes, inputs.core_catalog.sha256),
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
    let mut lucide = Vec::new();
    for id in &inputs.component_ids {
        if id == "cargo:whisper-rs@0.16.0" {
            push_text_material(
                projection,
                "share/into-markdown/licenses/whisper-rs-Unlicense.txt",
                "third_party/whisper-rs-0.16.0/LICENSE",
                vec![id.clone()],
                &["Unlicense"],
            );
        } else if id.starts_with("cargo:") {
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
        } else if id == "npm:lucide-react@1.31.0" {
            lucide.push(id.clone());
        } else if id.starts_with("npm:") {
            npm.push(id.clone());
        }
    }
    if !lucide.is_empty() {
        push_text_material(
            projection,
            "share/into-markdown/licenses/npm/lucide-ISC-MIT.txt",
            "third_party/licenses/npm/lucide-ISC-MIT.txt",
            lucide,
            &["ISC", "MIT"],
        );
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
            "opencc-transcript-character-table",
            "share/into-markdown/licenses/opencc-Apache-2.0.txt",
            "LICENSE",
            "Apache-2.0",
        ),
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
    if inputs.component_ids.iter().any(|id| id == "whisper-small") {
        push_text_material(
            projection,
            "share/into-markdown/licenses/whisper-model-MIT.txt",
            "third_party/licenses/whisper-model-MIT.txt",
            vec!["whisper-small".to_owned()],
            &["MIT"],
        );
    }
    if inputs.component_ids.iter().any(|id| id == "silero-vad-half-onnx-model") {
        push_text_material(
            projection,
            "share/into-markdown/licenses/silero-vad-MIT.txt",
            "third_party/licenses/silero-vad-MIT.txt",
            vec!["silero-vad-half-onnx-model".to_owned()],
            &["MIT"],
        );
    }
    if inputs.component_ids.iter().any(|id| id == "3dspeaker-eres2net-base-onnx-model") {
        push_text_material(
            projection,
            "share/into-markdown/licenses/3dspeaker-Apache-2.0.txt",
            "LICENSE",
            vec!["3dspeaker-eres2net-base-onnx-model".to_owned()],
            &["Apache-2.0"],
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
        for required in [
            "opencc-transcript-character-table",
            "imageproc-contour-adaptation",
            "clipper2-rust",
            "calamine",
        ] {
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
fn full_offline_whisper_projection_is_hash_and_license_bound() {
    let selected = vec!["whisper-small".to_owned()];
    let mut files = vec![ArchiveFile {
        path: "share/into-markdown/models/ggml-small.bin".into(),
        bytes: 487_601_967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b".into(),
        kind: ArchiveFileKind::Component,
        component_id: Some("whisper-small".into()),
        embedded_components: vec![],
    }];
    let mut errors = Vec::new();
    crate::models_fixtures::validate(&root(), &selected, &files, &mut errors);
    assert!(errors.is_empty(), "whisper projection baseline: {errors:?}");
    let evidence =
        crate::native::integrity(&root(), "x86_64-unknown-linux-gnu", "whisper-small", &mut errors);
    assert_eq!(evidence.len(), 2);
    assert!(errors.is_empty(), "whisper SBOM evidence: {errors:?}");

    let mut projection = ArchiveProjection {
        schema_version: 1,
        target: "x86_64-unknown-linux-gnu".into(),
        components: selected.clone(),
        files: files.clone(),
        license_materials: vec![],
        ffmpeg_evidence: None,
        native_transformations: vec![],
    };
    push_text_material(
        &mut projection,
        "share/into-markdown/licenses/whisper-model-MIT.txt",
        "third_party/licenses/whisper-model-MIT.txt",
        selected.clone(),
        &["MIT"],
    );
    let component = crate::schema::Component {
        id: "whisper-small".into(),
        kind: "model".into(),
        status: "reviewed".into(),
        included_in_release: false,
        release_eligible: true,
        manual_only: false,
        required_in_core: false,
        version: Some("pinned".into()),
        source: Some("https://huggingface.co/ggerganov/whisper.cpp".into()),
        license: Some("MIT".into()),
        obligations: Some("preserve license".into()),
        integrity: vec![],
        authority: "models/manifest.json".into(),
    };
    crate::materials::validate(&root(), &projection, &[&component], &mut errors);
    assert!(errors.is_empty(), "whisper license material: {errors:?}");

    files[0].sha256 = "0".repeat(64);
    let mut errors = Vec::new();
    crate::models_fixtures::validate(&root(), &selected, &files, &mut errors);
    assert!(errors.iter().any(|error| error.contains("whisper-small model")));
}

#[test]
fn full_offline_diarization_projection_is_hash_and_license_bound() {
    let selected = vec![
        "silero-vad-half-onnx-model".to_owned(),
        "3dspeaker-eres2net-base-onnx-model".to_owned(),
    ];
    let mut files = vec![
        ArchiveFile {
            path: "bin/models/silero-vad-3dspeaker-eres2net/silero_vad_half.onnx".into(),
            bytes: 1_280_395,
            sha256: "1e0b195ad4806595ef4466f419d16fca7e4afcfc6669b8c0b5f76ea87547c769".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("silero-vad-half-onnx-model".into()),
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "bin/models/silero-vad-3dspeaker-eres2net/3dspeaker_eres2net_base.onnx".into(),
            bytes: 39_593_761,
            sha256: "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b".into(),
            kind: ArchiveFileKind::Component,
            component_id: Some("3dspeaker-eres2net-base-onnx-model".into()),
            embedded_components: vec![],
        },
    ];
    let mut errors = Vec::new();
    crate::models_fixtures::validate(&root(), &selected, &files, &mut errors);
    assert!(errors.is_empty(), "diarization projection baseline: {errors:?}");

    files[1].sha256 = "0".repeat(64);
    crate::models_fixtures::validate(&root(), &selected, &files, &mut errors);
    assert!(errors.iter().any(|error| error.contains("3dspeaker-eres2net-base")));
}

#[test]
fn authoritative_runtime_closure_cannot_be_omitted_or_disguised() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    let omitted =
        projection.components.iter().find(|id| id.starts_with("cargo:")).cloned().unwrap();
    projection.components.retain(|id| id != &omitted);
    projection
        .files
        .iter_mut()
        .find(|file| file.kind == ArchiveFileKind::Project)
        .unwrap()
        .embedded_components
        .retain(|id| id != &omitted);
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
fn build_only_cargo_packages_are_not_release_runtime_components() {
    let inputs = generate_release_inputs(&root(), &request("aarch64-apple-darwin", &[])).unwrap();
    assert!(inputs.component_ids.iter().any(|id| id == "cargo:serde@1.0.229"));
    for package in [
        "autocfg@1.5.1",
        "cc@1.4.2",
        "find-msvc-tools@0.1.10",
        "phf_codegen@0.13.1",
        "phf_generator@0.13.1",
        "pkg-config@0.3.33",
        "shlex@2.0.1",
        "string_cache_codegen@0.6.1",
        "vcpkg@0.2.15",
        "version_check@0.9.5",
    ] {
        assert!(!inputs.component_ids.iter().any(|id| id == &format!("cargo:{package}")));
    }
    let errors =
        generate_release_inputs(&root(), &request("aarch64-apple-darwin", &["cargo:cc@1.4.2"]))
            .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("not release-eligible")));
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
fn smoke_and_rust_project_paths_are_narrowly_scoped() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.extend([
        ArchiveFile {
            path: "lib/into-markdown-rust/Cargo.toml".into(),
            bytes: 1,
            sha256: "a".repeat(64),
            kind: ArchiveFileKind::Project,
            component_id: None,
            embedded_components: vec![],
        },
        ArchiveFile {
            path: "share/into-markdown/smoke/fixtures/text/normal.txt".into(),
            bytes: 1,
            sha256: "b".repeat(64),
            kind: ArchiveFileKind::Project,
            component_id: None,
            embedded_components: vec![],
        },
    ]);
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();
    projection.files.push(ArchiveFile {
        path: "share/into-markdown/arbitrary-project-file".into(),
        bytes: 1,
        sha256: "c".repeat(64),
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
    generate_release_inputs(&root(), &request("aarch64-apple-darwin", &["ffmpeg"])).unwrap();
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
        authority_path: "bin/ffmpeg/authority.json".into(),
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
        path: "bin/ffmpeg/authority.json".into(),
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
    assert!(errors.iter().any(|error| error.contains("lacks a fixed SHA-256")));
}

#[test]
fn modular_core_catalog_is_an_allowed_project_release_file() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.push(ArchiveFile {
        path: "share/into-markdown/plugins/official-publisher.json".into(),
        bytes: 128,
        sha256: "a".repeat(64),
        kind: ArchiveFileKind::Project,
        component_id: None,
        embedded_components: vec![],
    });
    verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap()).unwrap();
}

#[test]
fn core_catalog_authority_is_mandatory_and_exact() {
    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.retain(|file| file.path != "core-catalog.json");
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("core-catalog.json")));

    let mut projection = minimal_projection("aarch64-apple-darwin");
    projection.files.iter_mut().find(|file| file.path == "core-catalog.json").unwrap().sha256 =
        "f".repeat(64);
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("does not match generated input")));
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

    projection
        .files
        .iter_mut()
        .find(|file| file.kind == ArchiveFileKind::Project)
        .unwrap()
        .embedded_components
        .push("cargo:unknown@9.9.9".into());
    let errors = verify_archive_projection(&root(), &serde_json::to_string(&projection).unwrap())
        .unwrap_err();
    assert!(errors.iter().any(|error| error.contains("embeds unknown or unselected")));
}
