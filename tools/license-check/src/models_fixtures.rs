//! Release bindings for model files; fixture-only inputs remain outside release projections.

use crate::schema::{ArchiveFile, ArchiveFileKind};
use serde_json::Value;
use std::fs;
use std::path::Path;

const MODEL_COMPONENT: &str = "ppocrv6-tiny-recognizer-onnx-model";
const TABLE_COMPONENT: &str = "ppocrv6-tiny-recognizer-character-table";

pub(crate) fn validate(
    repository: &Path,
    selected: &[String],
    files: &[ArchiveFile],
    errors: &mut Vec<String>,
) {
    if !selected
        .iter()
        .any(|component| component == MODEL_COMPONENT || component == TABLE_COMPONENT)
    {
        return;
    }
    let path = repository.join("models/ppocrv6-tiny-recognizer-authority.json");
    let authority = fs::read_to_string(&path)
        .map_err(|error| errors.push(format!("cannot read {}: {error}", path.display())))
        .ok()
        .and_then(|contents| {
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| errors.push(format!("invalid {}: {error}", path.display())))
                .ok()
        });
    let Some(authority) = authority else { return };
    for (id, size_key, hash_key) in [
        (MODEL_COMPONENT, "runtime_model_size", "runtime_model_sha256"),
        (TABLE_COMPONENT, "character_table_size", "character_table_sha256"),
    ] {
        if !selected.iter().any(|component| component == id) {
            continue;
        }
        let bytes = authority.get(size_key).and_then(Value::as_u64).unwrap_or_default();
        let sha256 = authority.get(hash_key).and_then(Value::as_str).unwrap_or_default();
        if !files.iter().any(|file| {
            file.kind == ArchiveFileKind::Component
                && file.component_id.as_deref() == Some(id)
                && file.bytes == bytes
                && file.sha256 == sha256
        }) {
            errors.push(format!(
                "projected model component {id} does not match fixed file authority"
            ));
        }
        for file in files.iter().filter(|file| file.component_id.as_deref() == Some(id)) {
            if file.kind != ArchiveFileKind::Component
                || file.bytes != bytes
                || file.sha256 != sha256
            {
                errors.push(format!(
                    "projected model component {id} contains a file outside its authority: {}",
                    file.path
                ));
            }
        }
    }
}
