//! Typed archive license-material verification without archive materialization.

use crate::schema::{ArchiveFileKind, ArchiveProjection, Component, LicenseMaterialKind};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub(crate) fn validate(
    repository: &Path,
    projection: &ArchiveProjection,
    components: &[&Component],
    errors: &mut Vec<String>,
) {
    let selected: BTreeSet<_> = components.iter().map(|item| item.id.as_str()).collect();
    let declared_paths: BTreeSet<_> =
        projection.license_materials.iter().map(|item| item.path.as_str()).collect();
    for file in projection.files.iter().filter(|file| file.kind == ArchiveFileKind::LicenseMaterial)
    {
        if !declared_paths.contains(file.path.as_str()) {
            errors
                .push(format!("archived license material {} has no typed declaration", file.path));
        }
    }
    for material in &projection.license_materials {
        let file = projection.files.iter().find(|file| file.path == material.path);
        if file.is_none_or(|file| {
            file.kind != ArchiveFileKind::LicenseMaterial
                || file.component_id.is_some()
                || file.bytes != material.bytes
                || file.sha256 != material.sha256
        }) {
            errors.push(format!(
                "license material {} is not bound to an archived file",
                material.path
            ));
        }
        if material.component_ids.is_empty() {
            errors.push(format!("license material {} covers no component", material.path));
        }
        let mut material_components = BTreeSet::new();
        for id in &material.component_ids {
            if !material_components.insert(id.as_str()) {
                errors
                    .push(format!("license material {} duplicates component {id}", material.path));
            }
            if !selected.contains(id.as_str()) {
                errors.push(format!(
                    "license material {} covers unknown component {id}",
                    material.path
                ));
            }
        }
        match material.contents.as_deref() {
            Some(contents) => {
                if material.bytes != contents.len() as u64
                    || material.sha256 != format!("{:x}", Sha256::digest(contents.as_bytes()))
                {
                    errors.push(format!(
                        "license material {} content hash or size differs",
                        material.path
                    ));
                }
            }
            None if material.kind == LicenseMaterialKind::LicenseText => {
                errors
                    .push(format!("license material {} lacks auditable full text", material.path));
            }
            None => {}
        }
    }
    for component in components {
        if !covers_component(repository, projection, component, errors) {
            errors.push(format!(
                "projected component {} lacks cryptographically fixed complete license material",
                component.id
            ));
        }
    }
    validate_ffmpeg(repository, projection, errors);
    validate_pdfium(repository, projection, errors);
}

fn covers_component(
    repository: &Path,
    projection: &ArchiveProjection,
    component: &Component,
    errors: &mut Vec<String>,
) -> bool {
    if component.id == "cargo:whisper-rs@0.16.0" {
        let expected = fs::read_to_string(repository.join("third_party/whisper-rs-0.16.0/LICENSE"))
            .unwrap_or_default();
        return projection.license_materials.iter().any(|item| {
            item.kind == LicenseMaterialKind::LicenseText
                && item.component_ids == [component.id.as_str()]
                && item.spdx_expressions == ["Unlicense"]
                && item.contents.as_deref() == Some(expected.as_str())
        });
    }
    if component.id == "cargo:whisper-rs-sys@0.15.0" {
        return exact_whisper_rs_sys_materials(repository, projection, component, errors);
    }
    if component.id.starts_with("cargo:") {
        let checksum = component
            .integrity
            .iter()
            .find(|item| {
                item.algorithm == "SHA-256" && item.subject.starts_with("crates.io archive ")
            })
            .map(|item| item.digest.as_str());
        return projection.license_materials.iter().any(|item| {
            item.kind == LicenseMaterialKind::UpstreamSourceArchive
                && item.component_ids == [component.id.as_str()]
                && Some(item.sha256.as_str()) == checksum
                && item.contents.is_none()
        });
    }
    if component.id == "ffmpeg" {
        return exact_source_material(repository, projection);
    }
    if component.id == "pdfium" {
        return exact_pdfium_bundle(repository, projection);
    }
    if component.id == "onnxruntime-cpu" {
        return exact_native_bundle(repository, projection, component, errors);
    }
    let authority_path = match component.id.as_str() {
        "opencc-transcript-character-table" => "LICENSE",
        "diagram-design-drawio-adaptation" => "third_party/licenses/diagram-design-MIT.txt",
        "imageproc-contour-adaptation" => "third_party/licenses/imageproc-MIT.txt",
        "clipper2-rust" => "third_party/licenses/BSL-1.0.txt",
        "calamine" => "third_party/licenses/calamine-MIT.txt",
        "npm:lucide-react@1.31.0" => "third_party/licenses/npm/lucide-ISC-MIT.txt",
        id if id.starts_with("npm:") => "third_party/licenses/npm/react-MIT.txt",
        "ppocrv6-tiny-recognizer-onnx-model"
        | "ppocrv6-tiny-recognizer-character-table"
        | "ppocrv6-tiny-detector-onnx-model"
        | "3dspeaker-eres2net-base-onnx-model" => "LICENSE",
        "whisper-small" => "third_party/licenses/whisper-model-MIT.txt",
        "silero-vad-half-onnx-model" => "third_party/licenses/silero-vad-MIT.txt",
        _ => return false,
    };
    let expected = fs::read_to_string(repository.join(authority_path)).unwrap_or_default();
    let terms: BTreeSet<_> =
        component.license.as_deref().unwrap_or_default().split(" AND ").collect();
    projection.license_materials.iter().any(|item| {
        let declared: BTreeSet<_> = item.spdx_expressions.iter().map(String::as_str).collect();
        item.kind == LicenseMaterialKind::LicenseText
            && item.component_ids.iter().any(|id| id == &component.id)
            && item.contents.as_deref() == Some(expected.as_str())
            && declared == terms
    })
}

fn exact_whisper_rs_sys_materials(
    repository: &Path,
    projection: &ArchiveProjection,
    component: &Component,
    errors: &mut Vec<String>,
) -> bool {
    let Some(authority) = crate::release_authority::whisper_rs_sys(repository, errors) else {
        return false;
    };
    let unlicense = fs::read_to_string(repository.join("third_party/whisper-rs-0.16.0/LICENSE"))
        .unwrap_or_default();
    let whisper_cpp = fs::read_to_string(
        repository.join("third_party/whisper-rs-0.16.0/sys/whisper.cpp/LICENSE"),
    )
    .unwrap_or_default();
    let source = projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::UpstreamSourceArchive
            && item.component_ids == [component.id.as_str()]
            && item.path == authority.archive_path
            && item.bytes == authority.bytes
            && item.sha256 == authority.sha256
            && item.contents.is_none()
    });
    let license = |path: &str, spdx: &str, expected: &str| {
        projection.license_materials.iter().any(|item| {
            item.kind == LicenseMaterialKind::LicenseText
                && item.component_ids == [component.id.as_str()]
                && item.path == path
                && item.spdx_expressions == [spdx]
                && item.contents.as_deref() == Some(expected)
        })
    };
    let integrity: BTreeSet<_> = component
        .integrity
        .iter()
        .filter(|item| item.algorithm == "SHA-256")
        .map(|item| item.digest.as_str())
        .collect();
    source
        && component.license.as_deref() == Some("Unlicense AND MIT")
        && integrity
            == BTreeSet::from([authority.crates_io_sha256.as_str(), authority.sha256.as_str()])
        && license(
            "share/into-markdown/licenses/whisper-rs-sys-Unlicense.txt",
            "Unlicense",
            &unlicense,
        )
        && license("share/into-markdown/licenses/whisper.cpp-MIT.txt", "MIT", &whisper_cpp)
}

fn exact_source_material(repository: &Path, projection: &ArchiveProjection) -> bool {
    let source: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/ffmpeg/source.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::CorrespondingSource
            && item.component_ids == ["ffmpeg"]
            && source.get("source_bytes").and_then(serde_json::Value::as_u64) == Some(item.bytes)
            && source.get("source_sha256").and_then(serde_json::Value::as_str)
                == Some(item.sha256.as_str())
    })
}

fn exact_native_bundle(
    repository: &Path,
    projection: &ArchiveProjection,
    component: &Component,
    errors: &mut Vec<String>,
) -> bool {
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/onnxruntime/manifest.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let Some(target) = manifest.pointer(&format!("/targets/{}", projection.target)) else {
        errors.push(format!("{} has no target license bundle authority", component.id));
        return false;
    };
    projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::NoticeBundle
            && item.component_ids == [component.id.as_str()]
            && target.get("sha256").and_then(serde_json::Value::as_str)
                == Some(item.sha256.as_str())
    })
}

fn validate_ffmpeg(repository: &Path, projection: &ArchiveProjection, errors: &mut Vec<String>) {
    if !projection.components.iter().any(|id| id == "ffmpeg") {
        return;
    }
    let source: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/ffmpeg/source.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let source_bytes = source.get("source_bytes").and_then(serde_json::Value::as_u64);
    let source_hash = source.get("source_sha256").and_then(serde_json::Value::as_str);
    let source_ok = projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::CorrespondingSource
            && item.component_ids == ["ffmpeg"]
            && Some(item.bytes) == source_bytes
            && Some(item.sha256.as_str()) == source_hash
    });
    let relink_ok = projection.license_materials.iter().any(|item| {
        let evidence = projection.ffmpeg_evidence.as_ref();
        item.kind == LicenseMaterialKind::RelinkMaterial
            && item.component_ids == ["ffmpeg"]
            && evidence.is_some_and(|evidence| {
                item.bytes == evidence.relink_bytes && item.sha256 == evidence.relink_sha256
            })
            && item.contents.is_none()
    });
    if !source_ok || !relink_ok {
        errors
            .push("FFmpeg archive lacks exact corresponding source or relink material".to_owned());
    }
}

fn validate_pdfium(repository: &Path, projection: &ArchiveProjection, errors: &mut Vec<String>) {
    if !projection.components.iter().any(|id| id == "pdfium") {
        return;
    }
    if !exact_pdfium_bundle(repository, projection) {
        errors
            .push("PDFium archive lacks its exact upstream full license/notice bundle".to_owned());
    }
}

fn exact_pdfium_bundle(repository: &Path, projection: &ArchiveProjection) -> bool {
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/pdfium/manifest.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let target = manifest.pointer(&format!("/targets/{}", projection.target));
    projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::NoticeBundle
            && item.component_ids == ["pdfium"]
            && target.is_some_and(|target| {
                target.get("archive_size").and_then(serde_json::Value::as_u64) == Some(item.bytes)
                    && target.get("archive_sha256").and_then(serde_json::Value::as_str)
                        == Some(item.sha256.as_str())
            })
    })
}

pub(crate) fn embedded_only(id: &str) -> bool {
    id.starts_with("cargo:")
        || id.starts_with("npm:")
        || matches!(
            id,
            "opencc-transcript-character-table"
                | "imageproc-contour-adaptation"
                | "diagram-design-drawio-adaptation"
                | "clipper2-rust"
                | "calamine"
        )
}

pub(crate) fn validate_release_file_collection(
    root_build: &str,
    npm_license_files: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    let Some(name_offset) = root_build.find("name = \"release_license_files\"") else {
        errors.push("root BUILD has no release_license_files authority".to_owned());
        return;
    };
    let Some(srcs_relative) = root_build[name_offset..].find("srcs = [") else {
        errors.push("release_license_files has no literal srcs list".to_owned());
        return;
    };
    let list_start = name_offset + srcs_relative + "srcs = [".len();
    let Some(list_length) = root_build[list_start..].find(']') else {
        errors.push("release_license_files srcs list is unterminated".to_owned());
        return;
    };
    let mut actual = BTreeSet::new();
    for line in root_build[list_start..list_start + list_length].lines() {
        let entry = line.trim().trim_end_matches(',');
        if entry.is_empty() {
            continue;
        }
        let Some(label) = entry.strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
            errors.push("release_license_files must contain only literal labels".to_owned());
            continue;
        };
        let Some(path) = release_label_path(label) else {
            errors.push(format!("release_license_files contains invalid label {label}"));
            continue;
        };
        if !actual.insert(path.clone()) {
            errors.push(format!("release_license_files duplicates {path}"));
        }
    }

    let mut expected = BTreeSet::from([
        "LICENSE".to_owned(),
        "NOTICE".to_owned(),
        "THIRD_PARTY_NOTICES.md".to_owned(),
        "third_party/licenses/npm-release.spdx.json".to_owned(),
        "third_party/licenses/diagram-design-MIT.txt".to_owned(),
    ]);
    expected.extend(npm_license_files.iter().map(|path| (*path).to_owned()));
    for path in expected.difference(&actual) {
        errors.push(format!("release_license_files is missing required {path}"));
    }
    for path in actual.difference(&expected) {
        errors.push(format!("release_license_files has unmanaged entry {path}"));
    }
}

fn release_label_path(label: &str) -> Option<String> {
    if let Some(absolute) = label.strip_prefix("//") {
        let (package, target) = absolute.split_once(':')?;
        if package.is_empty() || target.is_empty() {
            return None;
        }
        let path = format!("{package}/{target}");
        crate::is_safe_relative_path(&path).then_some(path)
    } else {
        crate::is_safe_relative_path(label).then(|| label.to_owned())
    }
}
