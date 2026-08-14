//! Official PP-OCRv6 tiny detector model and manifest authority.

use crate::runtime::{
    Dimension, ModelContract, ModelIdentity, ResolvedModel, TensorElementType, TensorSpec,
};
use crate::{ModelManager, ModelManagerError, ModelManifest, RuntimeArtifact};
use into_markdown_core::{ConversionError, ExecutionContext};
use serde::Deserialize;
use std::collections::BTreeMap;

pub(crate) const DETECTOR_MODEL_ID: &str = "pp-ocrv6-tiny-detector-onnx";
pub(crate) const PIPELINE_ID: &str = "pp-ocrv6-tiny-zh-en";
pub(crate) const PIPELINE_COMPONENTS: [&str; 2] =
    [DETECTOR_MODEL_ID, "pp-ocrv6-tiny-recognizer-onnx"];

pub(crate) fn pipeline_components(id: &str) -> Option<&'static [&'static str]> {
    (id == PIPELINE_ID).then_some(&PIPELINE_COMPONENTS)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    model_id: String,
    pipeline_id: String,
    upstream_repository: String,
    upstream_commit: String,
    preprocess_reference: String,
    postprocess_reference: String,
    runtime_archive_url: String,
    runtime_archive_size: u64,
    runtime_archive_sha256: String,
    runtime_model_member: String,
    runtime_model_size: u64,
    runtime_model_sha256: String,
    runtime_config_member: String,
    runtime_config_size: u64,
    runtime_config_sha256: String,
    license: String,
    ir_version: i64,
    opset_domain: String,
    opset_version: i64,
    input_name: String,
    input_dtype: String,
    input_shape: Vec<String>,
    output_name: String,
    output_dtype: String,
    output_shape: Vec<String>,
    input_color: String,
    minimum_side: usize,
    maximum_side: usize,
    resize_stride: usize,
    normalization_scale: f64,
    normalization_mean: [f32; 3],
    normalization_standard_deviation: [f32; 3],
    bitmap_threshold: f32,
    box_threshold: f32,
    maximum_candidates: usize,
    unclip_ratio: f64,
}

fn authority() -> Result<Authority, ConversionError> {
    let value: Authority = serde_json::from_str(include_str!(
        "../../../models/ppocrv6-tiny-detector-onnx-authority.json"
    ))
    .map_err(|_| invalid("detector authority JSON is invalid"))?;
    if value.schema_version != 1
        || value.model_id != DETECTOR_MODEL_ID
        || value.pipeline_id != PIPELINE_ID
        || value.upstream_repository != "https://github.com/PaddlePaddle/PaddleOCR"
        || value.upstream_commit != "2661c7c0ef5c613e8f93c6e93b2e052399f0f854"
        || value.preprocess_reference != "tools/infer/predict_det.py"
        || value.postprocess_reference != "ppocr/postprocess/db_postprocess.py"
        || value.license != "Apache-2.0"
        || value.ir_version != 10
        || !value.opset_domain.is_empty()
        || value.opset_version != 14
        || value.input_name != "x"
        || value.output_name != "fetch_name_0"
        || value.input_dtype != "float32"
        || value.output_dtype != "float32"
        || value.input_shape != ["N", "3", "H", "W"]
        || value.output_shape != ["N", "1", "H", "W"]
        || value.input_color != "BGR"
        || value.minimum_side != 736
        || value.maximum_side != 4000
        || value.resize_stride != 32
        || value.normalization_scale.to_bits() != (1.0_f64 / 255.0).to_bits()
        || value.normalization_mean.map(f32::to_bits) != [0.485_f32, 0.456, 0.406].map(f32::to_bits)
        || value.normalization_standard_deviation.map(f32::to_bits)
            != [0.229_f32, 0.224, 0.225].map(f32::to_bits)
        || value.bitmap_threshold.to_bits() != 0.2_f32.to_bits()
        || value.box_threshold.to_bits() != 0.4_f32.to_bits()
        || value.maximum_candidates != 3000
        || value.unclip_ratio.to_bits() != 1.4_f64.to_bits()
    {
        return Err(invalid("detector authority contract drift"));
    }
    Ok(value)
}

pub(crate) fn validate_manifest_authority(
    manifest: &ModelManifest,
    components: &BTreeMap<String, Vec<String>>,
) -> Result<(), ConversionError> {
    let expected = authority()?;
    let pipeline = manifest
        .bundles
        .iter()
        .find(|bundle| bundle.id == expected.pipeline_id)
        .ok_or_else(|| invalid("official OCR pipeline is absent"))?;
    if pipeline.availability != "available"
        || components
            .get(&pipeline.id)
            .is_none_or(|values| values.iter().map(String::as_str).ne(PIPELINE_COMPONENTS))
    {
        return Err(invalid("official OCR pipeline component authority drift"));
    }
    let bundle = manifest
        .bundles
        .iter()
        .find(|bundle| bundle.id == expected.model_id)
        .ok_or_else(|| invalid("official detector component is absent"))?;
    if bundle.kind != "detector-component"
        || bundle.availability != "available"
        || bundle.upstream_version
            != format!("PP-OCRv6 tiny / PaddleOCR {}", expected.upstream_commit)
        || bundle.runtime_format != "onnx"
        || components.get(&bundle.id).is_some_and(|value| !value.is_empty())
        || bundle.source_artifacts.len() != 1
        || bundle.runtime_artifacts.len() != 1
    {
        return Err(invalid("official detector component authority drift"));
    }
    let source = &bundle.source_artifacts[0];
    if source.id != "ppocrv6-tiny-detector-onnx-source"
        || source.role != "detector"
        || source.url != expected.runtime_archive_url
        || source.sha256 != expected.runtime_archive_sha256
        || source.format != "onnx-inference-tar"
        || source.license != expected.license
    {
        return Err(invalid("official detector source authority drift"));
    }
    validate_artifact(&bundle.runtime_artifacts[0], &expected)
}

fn validate_artifact(model: &RuntimeArtifact, expected: &Authority) -> Result<(), ConversionError> {
    if model.id != "ppocrv6-tiny-detector-onnx-model"
        || model.role != "detector"
        || model.file_name != "inference.onnx"
        || model.url != expected.runtime_archive_url
        || model.archive_sha256.as_deref() != Some(expected.runtime_archive_sha256.as_str())
        || model.archive_size != Some(expected.runtime_archive_size)
        || model.archive_member.as_deref() != Some(expected.runtime_model_member.as_str())
        || model.sha256 != expected.runtime_model_sha256
        || model.size != expected.runtime_model_size
        || model.license != expected.license
    {
        return Err(invalid("official detector model authority drift"));
    }
    let members = model
        .archive_members
        .as_deref()
        .ok_or_else(|| invalid("official detector archive authority is absent"))?;
    if members.len() != 3
        || members[1].path != expected.runtime_model_member
        || members[1].size != expected.runtime_model_size
        || members[1].sha256.as_deref() != Some(expected.runtime_model_sha256.as_str())
        || members[2].path != expected.runtime_config_member
        || members[2].size != expected.runtime_config_size
        || members[2].sha256.as_deref() != Some(expected.runtime_config_sha256.as_str())
    {
        return Err(invalid("official detector archive member authority drift"));
    }
    Ok(())
}

pub(crate) fn resolve_installed(
    manager: &ModelManager,
    context: &ExecutionContext,
) -> Result<ResolvedModel, ConversionError> {
    let artifact = manager
        .verified_runtime_artifact(DETECTOR_MODEL_ID, "detector", context)
        .map_err(map_manager_error)?;
    validate_runtime_model_identity(
        DETECTOR_MODEL_ID,
        &artifact.sha256,
        artifact.bytes.len() as u64,
    )?;
    Ok(ResolvedModel {
        identity: ModelIdentity {
            canonical_path: artifact.path,
            sha256: artifact.sha256,
            bytes: artifact.bytes.len() as u64,
            file_identity: artifact.file_identity,
        },
        contract: ppocrv6_detector_contract(),
        bytes: artifact.bytes,
        memory_reservation: Some(artifact.memory_reservation),
    })
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
        return Err(ocr("detectorModelAuthorityMismatch"));
    }
    Ok(())
}

/// Exact graph boundary accepted for the official detector model.
#[must_use]
pub fn ppocrv6_detector_contract() -> ModelContract {
    ModelContract {
        ir_version: 10,
        opsets: BTreeMap::from([(String::new(), 14)]),
        inputs: vec![TensorSpec {
            name: "x".into(),
            element_type: TensorElementType::Float32,
            dimensions: vec![
                Dimension::Dynamic { min: 1, max: 1 },
                Dimension::Exact(3),
                Dimension::Dynamic { min: 32, max: 4000 },
                Dimension::Dynamic { min: 32, max: 4000 },
            ],
        }],
        overridable_inputs: Vec::new(),
        outputs: vec![TensorSpec {
            name: "fetch_name_0".into(),
            element_type: TensorElementType::Float32,
            dimensions: vec![
                Dimension::Dynamic { min: 1, max: 1 },
                Dimension::Exact(1),
                Dimension::Dynamic { min: 32, max: 4000 },
                Dimension::Dynamic { min: 32, max: 4000 },
            ],
        }],
        session_memory_bytes: 128 * 1024 * 1024,
        run_memory_bytes: 128 * 1024 * 1024,
    }
}

fn map_manager_error(error: ModelManagerError) -> ConversionError {
    match error {
        ModelManagerError::Execution(error) => error,
        ModelManagerError::UnknownBundle
        | ModelManagerError::ComponentUnavailable
        | ModelManagerError::NotInstalled => ConversionError::ComponentUnavailable {
            component: DETECTOR_MODEL_ID.into(),
            detail: "installed detector model is unavailable".into(),
        },
        ModelManagerError::Corrupt(_)
        | ModelManagerError::DataDirectoryUnsafe
        | ModelManagerError::UnsafePath => ocr("installedDetectorModelCorrupt"),
        _ => ConversionError::ComponentUnavailable {
            component: DETECTOR_MODEL_ID.into(),
            detail: "installed detector model cannot be opened".into(),
        },
    }
}

fn invalid(detail: impl Into<String>) -> ConversionError {
    crate::invalid_manifest(detail)
}

fn ocr(detail: &str) -> ConversionError {
    ConversionError::Ocr { provider: "builtin.ocr.ppocrv6-detector".into(), detail: detail.into() }
}
