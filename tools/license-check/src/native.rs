//! Target-specific native runtime bindings used by archive projection verification.

use crate::schema::{ArchiveFile, ArchiveFileKind};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(crate) fn validate(
    repository: &Path,
    target: &str,
    components: &[String],
    files: &[ArchiveFile],
    errors: &mut Vec<String>,
) {
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
            errors,
        );
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
    errors: &mut Vec<String>,
) {
    let Some(manifest) = read_json(&repository.join(manifest_path), errors) else { return };
    let Some(authority) = manifest.pointer(&format!("/targets/{target}")) else {
        errors.push(format!("{component} has no authority for target {target}"));
        return;
    };
    let size = authority.get(size_field).and_then(Value::as_u64).unwrap_or_default();
    let hash = authority.get(library_hash_field).and_then(Value::as_str).unwrap_or_default();
    if !files.iter().any(|file| {
        file.kind == ArchiveFileKind::Component
            && file.component_id.as_deref() == Some(component)
            && file.bytes == size
            && file.sha256 == hash
    }) {
        errors.push(format!(
            "projected {component} file does not match target library size and SHA-256"
        ));
    }
    for file in files.iter().filter(|file| file.component_id.as_deref() == Some(component)) {
        if file.kind != ArchiveFileKind::Component || file.bytes != size || file.sha256 != hash {
            errors.push(format!(
                "projected {component} contains a file outside its target authority: {}",
                file.path
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
