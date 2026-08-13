//! AI provider contracts, bounded direct transport, and placeholder implementations.

mod openai;

pub use openai::{
    GenerationEndpoint, GenerationInput, GenerationRequest, GenerationResult,
    OpenAiCompatibleClient, ProviderConfig, ProviderError, ProviderErrorCode,
    ProviderNetworkPolicy, ProviderTestResult,
};

use into_markdown_core::{
    AiCapability, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionError, ExecutionContext,
    ExecutionStage, Transcriber, TranscriptionRequest, TranscriptionResult,
};
use std::collections::BTreeSet;

/// Configuration shape for future OpenAI-compatible multimodal providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatibleConfig {
    /// Provider ID used in provenance.
    pub provider_id: String,
    /// Base API URL.
    pub base_url: String,
    /// Model ID.
    pub model: String,
    /// Environment variable containing the secret API key.
    pub api_key_environment_variable: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Provider descriptor shown by the CLI without loading secrets or networking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiProviderDescriptor {
    /// Stable provider ID.
    pub id: &'static str,
    /// Implementation status.
    pub status: &'static str,
}

/// Planned provider adapters.
#[must_use]
pub fn planned_providers() -> &'static [AiProviderDescriptor] {
    &[
        AiProviderDescriptor { id: "openai-compatible", status: "available" },
        AiProviderDescriptor { id: "process-plugin-v1", status: "planned" },
        AiProviderDescriptor { id: "wasi-plugin-v1", status: "planned" },
    ]
}

/// A provider that advertises no capability and performs no I/O.
#[derive(Debug, Default)]
pub struct PlaceholderAiProvider;

impl AiProvider for PlaceholderAiProvider {
    fn id(&self) -> &'static str {
        "builtin.ai.placeholder"
    }

    fn capabilities(&self) -> BTreeSet<AiCapability> {
        BTreeSet::new()
    }

    fn execute<'a>(
        &'a self,
        _: AiRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            context.report(ExecutionStage::Ai, None, None, Some("builtin.ai.placeholder"))?;
            Err(ConversionError::Ai {
                provider: "builtin.ai.placeholder".into(),
                detail: "AI provider networking is not implemented in the scaffold".into(),
            })
        })
    }
}

/// A transcriber placeholder that performs no local or remote work.
#[derive(Debug, Default)]
pub struct PlaceholderTranscriber;

impl Transcriber for PlaceholderTranscriber {
    fn id(&self) -> &'static str {
        "builtin.transcriber.placeholder"
    }

    fn transcribe<'a>(
        &'a self,
        _: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            context.report(
                ExecutionStage::Ai,
                None,
                None,
                Some("builtin.transcriber.placeholder"),
            )?;
            Err(ConversionError::Ai {
                provider: "builtin.transcriber.placeholder".into(),
                detail: "audio transcription is not implemented in the scaffold".into(),
            })
        })
    }
}
