//! PP-OCRv6 recognizer model identity and runtime contract.

use crate::runtime::{
    Dimension, ModelContract, ModelIdentity, ModelResolver, ResolvedModel, TensorElementType,
    TensorSpec,
};
use crate::{ModelManager, ModelManagerError};
use into_markdown_core::{ConversionError, ExecutionContext};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const RECOGNIZER_MODEL_ID: &str = "pp-ocrv6-tiny-recognizer-onnx";

/// Product resolver backed by the installed, manager-verified model component.
pub struct ManifestModelResolver {
    manager: Arc<ModelManager>,
}

impl ManifestModelResolver {
    #[must_use]
    pub fn new(manager: Arc<ModelManager>) -> Self {
        Self { manager }
    }
}

impl ModelResolver for ManifestModelResolver {
    fn resolve(
        &self,
        model_id: &str,
        context: &ExecutionContext,
    ) -> Result<ResolvedModel, ConversionError> {
        context.checkpoint()?;
        if model_id != RECOGNIZER_MODEL_ID {
            return Err(unavailable("UnknownModel"));
        }
        let artifact = self
            .manager
            .verified_runtime_artifact(model_id, "recognizer", context)
            .map_err(map_manager_error)?;
        let bytes_len = u64::try_from(artifact.bytes.len()).map_err(|_| resource("modelBytes"))?;
        crate::recognition::model_authority::validate_runtime_model_identity(
            model_id,
            &artifact.sha256,
            bytes_len,
        )?;
        Ok(ResolvedModel {
            identity: ModelIdentity {
                canonical_path: artifact.path,
                sha256: artifact.sha256,
                bytes: bytes_len,
                file_identity: artifact.file_identity,
            },
            contract: ppocrv6_recognizer_contract(),
            bytes: artifact.bytes,
            memory_reservation: Some(artifact.memory_reservation),
        })
    }
}

pub(crate) fn map_manager_error(error: ModelManagerError) -> ConversionError {
    match error {
        ModelManagerError::Execution(error) => error,
        ModelManagerError::UnknownBundle
        | ModelManagerError::ComponentUnavailable
        | ModelManagerError::NotInstalled => unavailable("ModelUnavailable"),
        ModelManagerError::Corrupt(_)
        | ModelManagerError::DataDirectoryUnsafe
        | ModelManagerError::UnsafePath => ConversionError::Ocr {
            provider: "builtin.ocr.ppocrv6-recognizer".into(),
            detail: "installedRecognizerModelCorrupt".into(),
        },
        ModelManagerError::ReadOnly
        | ModelManagerError::DataDirectoryUnavailable
        | ModelManagerError::Busy
        | ModelManagerError::Io(_) => unavailable("ModelAccessFailed"),
    }
}

fn unavailable(detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable { component: "onnx-model".into(), detail: detail.into() }
}

fn resource(detail: &str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("ONNX model bound exceeded: {detail}"),
    }
}

#[must_use]
pub fn ppocrv6_recognizer_contract() -> ModelContract {
    ModelContract {
        ir_version: 6,
        opsets: BTreeMap::from([(String::new(), 11)]),
        inputs: vec![TensorSpec {
            name: "x".into(),
            element_type: TensorElementType::Float32,
            dimensions: vec![
                Dimension::Dynamic { min: 1, max: 8 },
                Dimension::Exact(3),
                Dimension::Exact(48),
                Dimension::Dynamic { min: 1, max: 3200 },
            ],
        }],
        overridable_inputs: Vec::new(),
        outputs: vec![TensorSpec {
            name: "fetch_name_0".into(),
            element_type: TensorElementType::Float32,
            dimensions: vec![
                Dimension::Dynamic { min: 1, max: 8 },
                Dimension::Dynamic { min: 1, max: 1024 },
                Dimension::Exact(6906),
            ],
        }],
        session_memory_bytes: 256 * 1024 * 1024,
        run_memory_bytes: 128 * 1024 * 1024,
    }
}
