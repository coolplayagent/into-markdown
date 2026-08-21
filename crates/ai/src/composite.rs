//! Exact-capability composition for adapters sharing one provider identity.

use into_markdown_core::{
    AiCapability, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionError, ConversionOptions,
    ExecutionContext,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// One configured provider assembled from non-overlapping typed adapters.
pub struct CompositeAiProvider {
    provider_id: String,
    capabilities: BTreeSet<AiCapability>,
    adapters: Vec<Arc<dyn AiProvider>>,
}

impl std::fmt::Debug for CompositeAiProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompositeAiProvider")
            .field("provider_id", &self.provider_id)
            .field("capabilities", &self.capabilities)
            .field("adapter_count", &self.adapters.len())
            .finish()
    }
}

impl CompositeAiProvider {
    /// Compose typed adapters which all carry the same provider identity and
    /// advertise disjoint capabilities.
    ///
    /// # Errors
    ///
    /// Rejects an empty set, identity mismatches, empty capability sets, and
    /// overlapping capability authority.
    pub fn new(
        provider_id: impl Into<String>,
        adapters: Vec<Arc<dyn AiProvider>>,
    ) -> Result<Self, ConversionError> {
        let provider_id = provider_id.into();
        if provider_id.is_empty() || adapters.is_empty() {
            return Err(invalid("provider identity and adapter set must be non-empty"));
        }
        let mut capabilities = BTreeSet::new();
        for adapter in &adapters {
            if adapter.id() != provider_id {
                return Err(invalid("adapter provider identity does not match the route"));
            }
            let advertised = adapter.capabilities();
            if advertised.is_empty() {
                return Err(invalid("adapter advertises no capabilities"));
            }
            for capability in advertised {
                if !capabilities.insert(capability) {
                    return Err(invalid("multiple adapters claim the same capability"));
                }
            }
        }
        Ok(Self { provider_id, capabilities, adapters })
    }

    fn adapter(&self, capability: AiCapability) -> Result<&dyn AiProvider, ConversionError> {
        self.adapters
            .iter()
            .find(|adapter| adapter.capabilities().contains(&capability))
            .map(AsRef::as_ref)
            .ok_or_else(|| ConversionError::ComponentUnavailable {
                component: self.provider_id.clone(),
                detail: "provider does not implement the requested capability".into(),
            })
    }
}

impl AiProvider for CompositeAiProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn capabilities(&self) -> BTreeSet<AiCapability> {
        self.capabilities.clone()
    }

    fn planned_output_bytes(
        &self,
        request: AiRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        self.adapter(request.capability)?.planned_output_bytes(request, options, context)
    }

    fn execute_with_options<'a>(
        &'a self,
        request: AiRequest<'a>,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        match self.adapter(request.capability) {
            Ok(adapter) => adapter.execute_with_options(request, options, context),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn execute<'a>(
        &'a self,
        request: AiRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        match self.adapter(request.capability) {
            Ok(adapter) => adapter.execute(request, context),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

fn invalid(detail: impl Into<String>) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "composite-ai-provider".into(),
        detail: detail.into(),
    }
}
