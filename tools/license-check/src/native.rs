//! Target-specific native runtime bindings used by archive projection verification.

use crate::schema::{
    ArchiveFile, ArchiveFileKind, IntegrityEvidence, NativeTransformation, NativeTransformationKind,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(crate) fn validate(
    repository: &Path,
    target: &str,
    components: &[String],
    files: &[ArchiveFile],
    transformations: &[NativeTransformation],
    errors: &mut Vec<String>,
) {
    validate_transformations(target, components, files, transformations, errors);
    if components.iter().any(|id| id == "onnxruntime-cpu") {
        validate_runtime(
            repository,
            "onnxruntime-cpu",
            "third_party/onnxruntime/manifest.json",
            target,
            "library_bytes",
            "library_sha256",
            "sha256",
            "native_archives",
            files,
            transformations,
            errors,
        );
    }
    if components.iter().any(|id| id == "pdfium") {
        validate_runtime(
            repository,
            "pdfium",
            "third_party/pdfium/manifest.json",
            target,
            "library_size",
            "library_sha256",
            "archive_sha256",
            "pdfium_archives",
            files,
            transformations,
            errors,
        );
    }
}

fn validate_transformations(
    target: &str,
    components: &[String],
    files: &[ArchiveFile],
    transformations: &[NativeTransformation],
    errors: &mut Vec<String>,
) {
    let expected_kind = match target {
        "aarch64-apple-darwin" => Some(NativeTransformationKind::AppleCodeSign),
        "x86_64-pc-windows-msvc" => Some(NativeTransformationKind::Authenticode),
        _ => None,
    };
    let mut subjects = HashSet::new();
    for transformation in transformations {
        let subject = (&transformation.component_id, &transformation.path);
        if !subjects.insert(subject) {
            errors.push(format!(
                "duplicate native transformation for {}:{}",
                transformation.component_id, transformation.path
            ));
        }
        if expected_kind != Some(transformation.kind) {
            errors.push(format!(
                "native transformation kind is invalid for target {target}: {}",
                transformation.path
            ));
        }
        if !components.contains(&transformation.component_id) {
            errors.push(format!(
                "native transformation references an unselected component: {}",
                transformation.component_id
            ));
        }
        if !matches!(transformation.component_id.as_str(), "pdfium" | "onnxruntime-cpu") {
            errors.push(format!(
                "native transformation has no fixed native authority: {}",
                transformation.component_id
            ));
        }
        let outputs = files
            .iter()
            .filter(|file| {
                file.kind == ArchiveFileKind::Component
                    && file.component_id.as_deref() == Some(transformation.component_id.as_str())
                    && file.path == transformation.path
                    && file.bytes == transformation.output_bytes
                    && file.sha256 == transformation.output_sha256
            })
            .count();
        if outputs != 1 {
            errors.push(format!(
                "native transformation output is not bound to exactly one projected file: {}:{}",
                transformation.component_id, transformation.path
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // Existing focused native tests remain near validation entrypoints.
mod tests {
    use super::*;

    fn pdfium_authority(target: &str) -> (u64, String) {
        let repository = crate::repository_root().unwrap();
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(repository.join("third_party/pdfium/manifest.json")).unwrap(),
        )
        .unwrap();
        let authority = manifest.pointer(&format!("/targets/{target}")).unwrap();
        (
            authority.get("library_size").and_then(Value::as_u64).unwrap(),
            authority.get("library_sha256").and_then(Value::as_str).unwrap().to_owned(),
        )
    }

    fn signed_pdfium() -> (Vec<String>, Vec<ArchiveFile>, Vec<NativeTransformation>) {
        let target = "aarch64-apple-darwin";
        let (source_bytes, source_sha256) = pdfium_authority(target);
        let file = ArchiveFile {
            path: "lib/pdfium/libpdfium.dylib".to_owned(),
            bytes: source_bytes + 512,
            sha1: None,
            sha256: "a".repeat(64),
            kind: ArchiveFileKind::Component,
            component_id: Some("pdfium".to_owned()),
            embedded_components: vec![],
        };
        let transformation = NativeTransformation {
            component_id: "pdfium".to_owned(),
            path: file.path.clone(),
            kind: NativeTransformationKind::AppleCodeSign,
            source_bytes,
            source_sha256,
            output_bytes: file.bytes,
            output_sha256: file.sha256.clone(),
        };
        (vec!["pdfium".to_owned()], vec![file], vec![transformation])
    }

    #[test]
    fn signed_native_file_is_bound_to_its_fixed_source() {
        let (components, files, transformations) = signed_pdfium();
        let mut errors = Vec::new();
        validate(
            &crate::repository_root().unwrap(),
            "aarch64-apple-darwin",
            &components,
            &files,
            &transformations,
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn signed_native_file_rejects_wrong_source_output_and_kind() {
        let (components, files, transformations) = signed_pdfium();
        for mutate in 0..3 {
            let mut candidate = transformations.clone();
            match mutate {
                0 => candidate[0].source_sha256 = "b".repeat(64),
                1 => candidate[0].output_sha256 = "c".repeat(64),
                _ => candidate[0].kind = NativeTransformationKind::Authenticode,
            }
            let mut errors = Vec::new();
            validate(
                &crate::repository_root().unwrap(),
                "aarch64-apple-darwin",
                &components,
                &files,
                &candidate,
                &mut errors,
            );
            assert!(!errors.is_empty(), "mutation {mutate} must fail closed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_runtime(
    repository: &Path,
    component: &str,
    manifest_path: &str,
    target: &str,
    size_field: &str,
    library_hash_field: &str,
    archive_hash_field: &str,
    downloads_group: &str,
    files: &[ArchiveFile],
    transformations: &[NativeTransformation],
    errors: &mut Vec<String>,
) {
    let Some(manifest) = read_json(&repository.join(manifest_path), errors) else { return };
    let Some(authority) = manifest.pointer(&format!("/targets/{target}")) else {
        errors.push(format!("{component} has no authority for target {target}"));
        return;
    };
    let size = authority.get(size_field).and_then(Value::as_u64).unwrap_or_default();
    let hash = authority.get(library_hash_field).and_then(Value::as_str).unwrap_or_default();
    let owned: Vec<_> =
        files.iter().filter(|file| file.component_id.as_deref() == Some(component)).collect();
    let valid = |file: &&ArchiveFile| {
        let fixed = file.bytes == size && file.sha256 == hash;
        let signed = transformations.iter().any(|transformation| {
            transformation.component_id == component
                && transformation.path == file.path
                && transformation.source_bytes == size
                && transformation.source_sha256 == hash
                && transformation.output_bytes == file.bytes
                && transformation.output_sha256 == file.sha256
        });
        file.kind == ArchiveFileKind::Component && (fixed || signed)
    };
    if !owned.iter().any(valid) {
        errors.push(format!(
            "projected {component} file does not match target library size and SHA-256"
        ));
    }
    for file in owned {
        if !valid(&file) {
            errors.push(format!(
                "projected {component} contains a file outside its target authority: {}",
                file.path
            ));
        }
    }
    for transformation in
        transformations.iter().filter(|transformation| transformation.component_id == component)
    {
        if transformation.source_bytes != size || transformation.source_sha256 != hash {
            errors.push(format!(
                "native transformation source does not match {component} target authority: {}",
                transformation.path
            ));
        }
    }
    let archive_hash =
        authority.get(archive_hash_field).and_then(Value::as_str).unwrap_or_default();
    let Some(downloads) =
        read_json(&repository.join("third_party/licenses/downloads.json"), errors)
    else {
        return;
    };
    let bound = downloads.get(downloads_group).and_then(Value::as_array).is_some_and(|items| {
        items.iter().any(|item| {
            item.get("target").and_then(Value::as_str) == Some(target)
                && item.get("sha256").and_then(Value::as_str) == Some(archive_hash)
        })
    });
    if !bound {
        errors.push(format!("projected {component} target is not bound to the download authority"));
    }
}

fn read_json(path: &Path, errors: &mut Vec<String>) -> Option<Value> {
    fs::read_to_string(path)
        .map_err(|error| errors.push(format!("cannot read {}: {error}", path.display())))
        .ok()
        .and_then(|contents| {
            serde_json::from_str(&contents)
                .map_err(|error| errors.push(format!("invalid {}: {error}", path.display())))
                .ok()
        })
}

pub(crate) fn integrity(
    repository: &Path,
    target: &str,
    component: &str,
    errors: &mut Vec<String>,
) -> Vec<IntegrityEvidence> {
    if component == "ffmpeg" {
        let mut evidence = crate::ffmpeg::integrity(repository, target, errors);
        if let Some(digest) = read_json(&repository.join("third_party/ffmpeg/source.json"), errors)
            .as_ref()
            .and_then(|value| value.get("source_sha256"))
            .and_then(Value::as_str)
        {
            evidence.push(IntegrityEvidence {
                algorithm: "SHA-256".to_owned(),
                digest: digest.to_owned(),
                subject: "ffmpeg source archive".to_owned(),
                target: Some(target.to_owned()),
            });
        }
        return evidence;
    }
    let (path, archive_field, binary_field, archive_subject, binary_subject) = match component {
        "onnxruntime-cpu" => (
            "third_party/onnxruntime/manifest.json",
            "sha256",
            "library_sha256",
            "download archive",
            "runtime library",
        ),
        "pdfium" => (
            "third_party/pdfium/manifest.json",
            "archive_sha256",
            "library_sha256",
            "download archive",
            "runtime library",
        ),
        _ => return model_integrity(repository, target, component, errors),
    };
    let Some(manifest) = read_json(&repository.join(path), errors) else { return Vec::new() };
    let Some(authority) = manifest.pointer(&format!("/targets/{target}")) else {
        errors.push(format!("{component} has no SBOM integrity for target {target}"));
        return Vec::new();
    };
    let mut result = Vec::new();
    for (field, subject) in [(archive_field, archive_subject), (binary_field, binary_subject)] {
        if let Some(value) = authority.get(field).and_then(Value::as_str) {
            let evidence = IntegrityEvidence {
                algorithm: "SHA-256".to_owned(),
                digest: value.to_owned(),
                subject: format!("{component} {subject}"),
                target: Some(target.to_owned()),
            };
            if !result.contains(&evidence) {
                result.push(evidence);
            }
        }
    }
    let bytes = fs::read(repository.join(path)).unwrap_or_default();
    result.push(IntegrityEvidence {
        algorithm: "SHA-256".to_owned(),
        digest: format!("{:x}", Sha256::digest(&bytes)),
        subject: format!("authority file {path}"),
        target: Some(target.to_owned()),
    });
    let group = if component == "onnxruntime-cpu" { "native_archives" } else { "pdfium_archives" };
    append_download_integrity(
        repository,
        target,
        component,
        group,
        archive_field,
        authority,
        &mut result,
        errors,
    );
    result
}

fn model_integrity(
    repository: &Path,
    target: &str,
    component: &str,
    errors: &mut Vec<String>,
) -> Vec<IntegrityEvidence> {
    if let Some((bundle, artifact)) = match component {
        "whisper-small" => Some(("whisper-small-multilingual", "whisper-small-model")),
        "silero-vad-half-onnx-model" => {
            Some(("silero-vad-3dspeaker-eres2net", "silero-vad-half-onnx-model"))
        }
        "3dspeaker-eres2net-base-onnx-model" => {
            Some(("silero-vad-3dspeaker-eres2net", "3dspeaker-eres2net-base-onnx-model"))
        }
        _ => None,
    } {
        return manifest_model_integrity(repository, target, component, bundle, artifact, errors);
    }
    let path = if component == "ppocrv6-tiny-detector-onnx-model" {
        "models/ppocrv6-tiny-detector-onnx-authority.json"
    } else {
        "models/ppocrv6-tiny-recognizer-authority.json"
    };
    let Some(authority) = read_json(&repository.join(path), errors) else { return Vec::new() };
    let fields: &[(&str, &str)] = match component {
        "ppocrv6-tiny-detector-onnx-model" | "ppocrv6-tiny-recognizer-onnx-model" => &[
            ("runtime_archive_sha256", "model download archive"),
            ("runtime_model_sha256", "model runtime member"),
        ],
        "ppocrv6-tiny-recognizer-character-table" => {
            &[("character_table_sha256", "character table")]
        }
        _ => return Vec::new(),
    };
    let mut result: Vec<_> = fields
        .iter()
        .filter_map(|(field, subject)| {
            authority.get(field).and_then(Value::as_str).map(|digest| IntegrityEvidence {
                algorithm: "SHA-256".to_owned(),
                digest: digest.to_owned(),
                subject: format!("{component} {subject}"),
                target: Some(target.to_owned()),
            })
        })
        .collect();
    let downloads = read_json(&repository.join("third_party/licenses/downloads.json"), errors);
    let item = downloads
        .as_ref()
        .and_then(|value| value.get("model_runtime_files"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("artifact_id").and_then(Value::as_str) == Some(component))
        });
    let manifest_digest = match component {
        "ppocrv6-tiny-detector-onnx-model" | "ppocrv6-tiny-recognizer-onnx-model" => {
            authority.get("runtime_model_sha256")
        }
        "ppocrv6-tiny-recognizer-character-table" => authority.get("character_table_sha256"),
        _ => None,
    }
    .and_then(Value::as_str);
    if let Some(item) = item
        && item.get("sha256").and_then(Value::as_str) == manifest_digest
    {
        result.push(IntegrityEvidence {
            algorithm: "SHA-256".to_owned(),
            digest: manifest_digest.unwrap_or_default().to_owned(),
            subject: format!("{component} controlled download file"),
            target: Some(target.to_owned()),
        });
    } else {
        errors.push(format!("{component} SBOM integrity is not bound to download authority"));
    }
    result
}

fn manifest_model_integrity(
    repository: &Path,
    target: &str,
    component: &str,
    bundle_id: &str,
    artifact_id: &str,
    errors: &mut Vec<String>,
) -> Vec<IntegrityEvidence> {
    let path = "models/manifest.json";
    let Some(manifest) = read_json(&repository.join(path), errors) else { return Vec::new() };
    let artifact = manifest
        .get("bundles")
        .and_then(Value::as_array)
        .and_then(|bundles| {
            bundles
                .iter()
                .find(|bundle| bundle.get("id").and_then(Value::as_str) == Some(bundle_id))
        })
        .and_then(|bundle| bundle.get("runtime_artifacts"))
        .and_then(Value::as_array)
        .and_then(|artifacts| {
            artifacts
                .iter()
                .find(|artifact| artifact.get("id").and_then(Value::as_str) == Some(artifact_id))
        });
    let digest = artifact.and_then(|artifact| artifact.get("sha256")).and_then(Value::as_str);
    let downloads = read_json(&repository.join("third_party/licenses/downloads.json"), errors);
    let controlled = downloads
        .as_ref()
        .and_then(|value| value.get("model_runtime_files"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("artifact_id").and_then(Value::as_str) == Some(artifact_id))
        });
    if digest.is_none()
        || controlled.and_then(|item| item.get("sha256")).and_then(Value::as_str) != digest
    {
        errors.push(format!("{component} SBOM integrity is not bound to model/download authority"));
        return Vec::new();
    }
    let bytes = fs::read(repository.join(path)).unwrap_or_default();
    vec![
        IntegrityEvidence {
            algorithm: "SHA-256".to_owned(),
            digest: digest.unwrap_or_default().to_owned(),
            subject: format!("{component} controlled model file"),
            target: Some(target.to_owned()),
        },
        IntegrityEvidence {
            algorithm: "SHA-256".to_owned(),
            digest: format!("{:x}", Sha256::digest(bytes)),
            subject: format!("authority file {path}"),
            target: Some(target.to_owned()),
        },
    ]
}

#[allow(clippy::too_many_arguments)]
fn append_download_integrity(
    repository: &Path,
    target: &str,
    component: &str,
    group: &str,
    archive_field: &str,
    manifest: &Value,
    result: &mut Vec<IntegrityEvidence>,
    errors: &mut Vec<String>,
) {
    let downloads = read_json(&repository.join("third_party/licenses/downloads.json"), errors);
    let item =
        downloads.as_ref().and_then(|value| value.get(group)).and_then(Value::as_array).and_then(
            |items| {
                items.iter().find(|item| item.get("target").and_then(Value::as_str) == Some(target))
            },
        );
    let digest = manifest.get(archive_field).and_then(Value::as_str);
    if let Some(item) = item
        && item.get("sha256").and_then(Value::as_str) == digest
    {
        result.push(IntegrityEvidence {
            algorithm: "SHA-256".to_owned(),
            digest: digest.unwrap_or_default().to_owned(),
            subject: format!("{component} controlled download archive"),
            target: Some(target.to_owned()),
        });
    } else {
        errors
            .push(format!("{component} SBOM integrity is not bound to target download authority"));
    }
}

pub(crate) const PDFIUM_REQUIRED_EXPORTS: [&str; 48] = [
    "FPDF_InitLibraryWithConfig",
    "FPDF_DestroyLibrary",
    "FPDF_LoadMemDocument64",
    "FPDF_CloseDocument",
    "FPDF_GetPageCount",
    "FPDF_LoadPage",
    "FPDF_ClosePage",
    "FPDFText_LoadPage",
    "FPDFText_ClosePage",
    "FPDFText_CountChars",
    "FPDFText_GetText",
    "FPDFText_GetUnicode",
    "FPDFText_GetCharBox",
    "FPDFText_GetFontSize",
    "FPDFText_GetFontInfo",
    "FPDFText_GetCharAngle",
    "FPDF_GetPageWidthF",
    "FPDF_GetPageHeightF",
    "FPDF_GetPageBoundingBox",
    "FPDFPage_GetRotation",
    "FPDFPage_CountObjects",
    "FPDFPage_GetObject",
    "FPDFPageObj_GetType",
    "FPDFPageObj_GetBounds",
    "FPDFLink_Enumerate",
    "FPDFLink_GetAnnotRect",
    "FPDFLink_GetAction",
    "FPDFAction_GetType",
    "FPDFAction_GetURIPath",
    "FPDFLink_GetDest",
    "FPDFDest_GetDestPageIndex",
    "FPDFLink_LoadWebLinks",
    "FPDFLink_CountWebLinks",
    "FPDFLink_GetURL",
    "FPDFLink_CountRects",
    "FPDFLink_GetRect",
    "FPDFLink_CloseWebLinks",
    "FPDFBitmap_CreateEx",
    "FPDFBitmap_Destroy",
    "FPDFBitmap_GetBuffer",
    "FPDFBitmap_GetFormat",
    "FPDFBitmap_GetHeight",
    "FPDFBitmap_GetStride",
    "FPDFBitmap_GetWidth",
    "FPDFImageObj_GetBitmap",
    "FPDFImageObj_GetImagePixelSize",
    "FPDF_RenderPageBitmap",
    "FPDF_GetLastError",
];
