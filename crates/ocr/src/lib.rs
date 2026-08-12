//! Local OCR model metadata and runtime seams.
//!
//! No inference implementation is present in the scaffold. Model source
//! archives are pinned by hash and are fetched only through manual Bazel
//! targets.

use into_markdown_core::{
    BoxFuture, ConversionError, OcrEngine, OcrRequest, OcrResult, Tensor, TensorRuntime,
};
use serde::{Deserialize, Serialize};

/// One downloadable model source artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelArtifact {
    /// Stable artifact ID.
    pub id: String,
    /// Pipeline role, such as `detector` or `recognizer`.
    pub role: String,
    /// HTTPS source URL.
    pub url: String,
    /// Lowercase SHA-256 digest.
    pub sha256: String,
    /// Upstream artifact format.
    pub format: String,
    /// SPDX license identifier.
    pub license: String,
}

/// OCR model bundle and its derivation contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBundle {
    /// Stable bundle ID.
    pub id: String,
    /// Human-readable model family/version.
    pub upstream_version: String,
    /// Languages/scripts expected from the bundle.
    pub languages: Vec<String>,
    /// Runtime format produced by the future conversion pipeline.
    pub runtime_format: String,
    /// Hash-pinned upstream inputs.
    pub source_artifacts: Vec<ModelArtifact>,
}

/// Versioned model manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Default bundle ID.
    pub default_bundle: String,
    /// Known bundles.
    pub bundles: Vec<ModelBundle>,
}

impl ModelManifest {
    /// Parse the repository's embedded manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Internal`] when embedded JSON is invalid or
    /// violates the model supply-chain contract.
    pub fn embedded() -> Result<Self, ConversionError> {
        let manifest: Self = serde_json::from_str(include_str!("../../../models/manifest.json"))
            .map_err(|error| ConversionError::Internal {
                detail: format!("invalid embedded model manifest: {error}"),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate schema, IDs, secure URLs, SPDX labels, and hashes.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Internal`] for an unsupported schema or an
    /// incomplete/insecure artifact declaration.
    pub fn validate(&self) -> Result<(), ConversionError> {
        if self.schema_version != 1 {
            return Err(invalid_manifest(format!(
                "unsupported schema version {}",
                self.schema_version
            )));
        }
        if !self.bundles.iter().any(|bundle| bundle.id == self.default_bundle) {
            return Err(invalid_manifest("default bundle is absent"));
        }
        for bundle in &self.bundles {
            if bundle.id.is_empty() || bundle.source_artifacts.is_empty() {
                return Err(invalid_manifest("bundle ID and source artifacts are required"));
            }
            for artifact in &bundle.source_artifacts {
                if !artifact.url.starts_with("https://") {
                    return Err(invalid_manifest(format!(
                        "artifact {} URL is not HTTPS",
                        artifact.id
                    )));
                }
                if artifact.sha256.len() != 64
                    || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(invalid_manifest(format!(
                        "artifact {} has invalid SHA-256",
                        artifact.id
                    )));
                }
                if artifact.license.is_empty() {
                    return Err(invalid_manifest(format!(
                        "artifact {} has no license",
                        artifact.id
                    )));
                }
            }
        }
        Ok(())
    }
}

fn invalid_manifest(detail: impl Into<String>) -> ConversionError {
    ConversionError::Internal { detail: format!("model manifest: {}", detail.into()) }
}

/// Non-inferencing OCR placeholder.
#[derive(Debug, Default)]
pub struct PlaceholderOcrEngine;

impl OcrEngine for PlaceholderOcrEngine {
    fn id(&self) -> &'static str {
        "builtin.ocr.placeholder"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async {
            Err(ConversionError::Ocr {
                provider: "builtin.ocr.placeholder".into(),
                detail: "PP-OCRv6 inference is not implemented in the scaffold".into(),
            })
        })
    }
}

/// Non-inferencing tensor-runtime placeholder.
#[derive(Debug, Default)]
pub struct PlaceholderTensorRuntime;

impl TensorRuntime for PlaceholderTensorRuntime {
    fn id(&self) -> &'static str {
        "builtin.tensor-runtime.placeholder"
    }

    fn run<'a>(
        &'a self,
        _: &'a str,
        _: &'a [Tensor],
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
        Box::pin(async {
            Err(ConversionError::Ocr {
                provider: "builtin.tensor-runtime.placeholder".into(),
                detail: "ONNX Runtime integration is not implemented in the scaffold".into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_valid_and_selects_tiny_model() {
        let manifest = ModelManifest::embedded().unwrap();
        assert_eq!(manifest.default_bundle, "pp-ocrv6-tiny-zh-en");
        let bundle = &manifest.bundles[0];
        assert!(bundle.languages.contains(&"zh-Hans".to_string()));
        assert!(bundle.languages.contains(&"zh-Hant".to_string()));
        assert!(bundle.languages.contains(&"en".to_string()));
    }

    #[test]
    fn invalid_hash_is_rejected() {
        let mut manifest = ModelManifest::embedded().unwrap();
        manifest.bundles[0].source_artifacts[0].sha256 = "bad".into();
        assert!(manifest.validate().is_err());
    }
}
