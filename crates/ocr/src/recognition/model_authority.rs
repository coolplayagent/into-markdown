//! Cross-authority binding for the installable official recognizer component.

use super::authority::authority;
use super::ocr;
use crate::{ModelManifest, RuntimeArtifact};
use into_markdown_core::ConversionError;

pub(crate) fn validate_manifest_authority(manifest: &ModelManifest) -> Result<(), ConversionError> {
    let expected = authority()?;
    let bundle = manifest
        .bundles
        .iter()
        .find(|bundle| bundle.id == expected.model_id)
        .ok_or_else(|| crate::invalid_manifest("official recognizer component is absent"))?;
    if bundle.kind != "recognizer-component"
        || bundle.availability != "available"
        || bundle.upstream_version
            != format!("PP-OCRv6 tiny / PaddleOCR {}", expected.upstream_commit)
        || bundle.runtime_format != "onnx"
        || bundle.character_set.as_ref().is_none_or(|character_set| {
            character_set.status != "available"
                || character_set.source_artifact_id != "ppocrv6-tiny-recognizer-onnx-source"
        })
        || bundle.source_artifacts.len() != 1
        || bundle.runtime_artifacts.len() != 2
    {
        return Err(crate::invalid_manifest("official recognizer component authority drift"));
    }
    let source = &bundle.source_artifacts[0];
    if source.id != "ppocrv6-tiny-recognizer-onnx-source"
        || source.role != "recognizer-and-dictionary"
        || source.url != expected.runtime_archive_url
        || source.sha256 != expected.runtime_archive_sha256
        || source.format != "onnx-inference-tar"
        || source.license != expected.license
    {
        return Err(crate::invalid_manifest("official recognizer source authority drift"));
    }
    let model = artifact_by_role(&bundle.runtime_artifacts, "recognizer")?;
    if model.id != "ppocrv6-tiny-recognizer-onnx-model"
        || model.file_name != "inference.onnx"
        || model.url != expected.runtime_archive_url
        || model.archive_sha256.as_deref() != Some(expected.runtime_archive_sha256.as_str())
        || model.archive_size != Some(expected.runtime_archive_size)
        || model.archive_member.as_deref() != Some(expected.runtime_model_member.as_str())
        || model.sha256 != expected.runtime_model_sha256
        || model.size != expected.runtime_model_size
        || model.license != expected.license
    {
        return Err(crate::invalid_manifest("official recognizer model authority drift"));
    }
    let members = model
        .archive_members
        .as_deref()
        .ok_or_else(|| crate::invalid_manifest("official recognizer archive authority absent"))?;
    if members.len() != 3
        || members[1].path != expected.runtime_model_member
        || members[1].size != expected.runtime_model_size
        || members[1].sha256.as_deref() != Some(expected.runtime_model_sha256.as_str())
        || members[2].path != expected.runtime_config_member
        || members[2].size != expected.runtime_config_size
        || members[2].sha256.as_deref() != Some(expected.runtime_config_sha256.as_str())
    {
        return Err(crate::invalid_manifest("official recognizer archive member drift"));
    }
    let dictionary = artifact_by_role(&bundle.runtime_artifacts, "character-table")?;
    if dictionary.id != "ppocrv6-tiny-recognizer-character-table"
        || dictionary.file_name != "ppocrv6_tiny_dict.txt"
        || dictionary.url != expected.character_table_url
        || dictionary.sha256 != expected.character_table_sha256
        || dictionary.size != expected.character_table_size
        || dictionary.license != expected.license
    {
        return Err(crate::invalid_manifest("official recognizer dictionary authority drift"));
    }
    Ok(())
}

pub(crate) fn validate_runtime_model_identity(
    model_id: &str,
    sha256: &str,
    size: u64,
) -> Result<(), ConversionError> {
    let expected = authority()?;
    if model_id != expected.model_id
        || sha256 != expected.runtime_model_sha256
        || size != expected.runtime_model_size
    {
        return Err(ocr("recognizerModelAuthorityMismatch"));
    }
    Ok(())
}

fn artifact_by_role<'a>(
    artifacts: &'a [RuntimeArtifact],
    role: &str,
) -> Result<&'a RuntimeArtifact, ConversionError> {
    artifacts
        .iter()
        .find(|artifact| artifact.role == role)
        .ok_or_else(|| crate::invalid_manifest("official recognizer runtime role is absent"))
}
