//! Policy-bound OpenAI-compatible image-description adapter.

use crate::{
    GenerationEndpoint, GenerationInput, GenerationRequest, OpenAiCompatibleClient, ProviderError,
    ProviderErrorCode, ProviderNetworkPolicy,
};
use into_markdown_core::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, Block, BlockNode, BoxFuture,
    ConversionError, ConversionOptions, ExecutionContext, Inline, NodeId, Provenance,
    ProvenanceKind, SourceLocator,
};
use std::collections::BTreeSet;

const PROVIDER_ID: &str = "openai-compatible.image-description";
const FIXED_PROMPT: &str = "Describe the visible content of this image accurately and concisely. Do not infer hidden text or metadata.";
const MAX_OUTPUT_TOKENS: u32 = 512;
const FIXED_WORKING_BYTES: u64 = 512 * 1024;

/// OpenAI-compatible multimodal provider restricted to fixed image descriptions.
pub struct OpenAiImageDescriptionProvider {
    client: OpenAiCompatibleClient,
    network: ProviderNetworkPolicy,
    provider_id: String,
}

impl std::fmt::Debug for OpenAiImageDescriptionProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiImageDescriptionProvider")
            .field("client", &self.client)
            .field("network", &self.network)
            .field("provider_id", &self.provider_id)
            .finish()
    }
}

impl OpenAiImageDescriptionProvider {
    /// Bind a validated transport to the exact invocation network policy.
    #[must_use]
    pub fn new(client: OpenAiCompatibleClient, network: ProviderNetworkPolicy) -> Self {
        Self { client, network, provider_id: PROVIDER_ID.into() }
    }

    /// Bind a transport to an exact configured provider identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or non-canonical provider IDs.
    pub fn new_with_id(
        client: OpenAiCompatibleClient,
        network: ProviderNetworkPolicy,
        provider_id: impl Into<String>,
    ) -> Result<Self, ConversionError> {
        let provider_id = provider_id.into();
        if provider_id.is_empty()
            || provider_id.len() > 512
            || !provider_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(ConversionError::ComponentUnavailable {
                component: "image-description-provider".into(),
                detail: "provider identity is invalid".into(),
            });
        }
        Ok(Self { client, network, provider_id })
    }
}

impl AiProvider for OpenAiImageDescriptionProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn capabilities(&self) -> BTreeSet<AiCapability> {
        BTreeSet::from([AiCapability::ImageDescription])
    }

    fn planned_output_bytes(
        &self,
        request: AiRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        context.checkpoint()?;
        let (bytes, media_type, page) = page_image(request, &self.provider_id)?;
        validate_request(request, media_type, page, options, &self.network, &self.provider_id)?;
        let encoded_peak = u64::try_from(bytes.len())
            .map_err(|_| memory("image size is unrepresentable"))?
            .checked_mul(5)
            .and_then(|value| value.checked_add(FIXED_WORKING_BYTES))
            .and_then(|value| value.checked_add(options.limits.max_field_bytes))
            .ok_or_else(|| memory("image-description plan overflow"))?;
        if encoded_peak > options.limits.max_memory_bytes
            || encoded_peak > context.available_memory_bytes()
        {
            return Err(memory("image-description plan exceeds request memory"));
        }
        Ok(encoded_peak)
    }

    fn execute_with_options<'a>(
        &'a self,
        request: AiRequest<'a>,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let (bytes, media_type, page) = page_image(request, &self.provider_id)?;
            validate_request(request, media_type, page, options, &self.network, &self.provider_id)?;
            let result = self
                .client
                .generate(
                    GenerationRequest {
                        endpoint: GenerationEndpoint::Responses,
                        capability: "image-description",
                        input: GenerationInput::Image { bytes, media_type, prompt: FIXED_PROMPT },
                        max_output_tokens: MAX_OUTPUT_TOKENS,
                        idempotency_key: None,
                    },
                    context,
                )
                .map_err(|error| map_provider_error(&self.provider_id, &error))?;
            let text = result.text.trim();
            if text.is_empty() || text.len() as u64 > options.limits.max_field_bytes {
                return Err(ConversionError::Ai {
                    provider: self.provider_id.clone(),
                    detail: "provider returned an empty or oversized image description".into(),
                });
            }
            let provenance = Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: self.provider_id.clone(),
                locator: SourceLocator { page: Some(page), ..SourceLocator::default() },
                confidence: None,
            };
            Ok(AiOutput {
                nodes: vec![BlockNode {
                    id: NodeId(format!("image-page-{page}-ai-description")),
                    block: Block::Paragraph(vec![Inline::Text {
                        value: text.to_owned(),
                        marks: vec![],
                    }]),
                    provenance,
                }],
                ..AiOutput::default()
            })
        })
    }

    fn execute<'a>(
        &'a self,
        _: AiRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Err(ConversionError::ComponentUnavailable {
                component: self.provider_id.clone(),
                detail: "policy-bound execution options are required".into(),
            })
        })
    }
}

fn page_image<'a>(
    request: AiRequest<'a>,
    provider: &str,
) -> Result<(&'a [u8], &'a str, u32), ConversionError> {
    match request.input {
        AiInput::PageImage { bytes, media_type, page } if page > 0 => Ok((bytes, media_type, page)),
        _ => Err(ConversionError::Ai {
            provider: provider.into(),
            detail: "image description requires a page-bound encoded image".into(),
        }),
    }
}

fn validate_request(
    request: AiRequest<'_>,
    media_type: &str,
    page: u32,
    options: &ConversionOptions,
    network: &ProviderNetworkPolicy,
    provider: &str,
) -> Result<(), ConversionError> {
    if request.capability != AiCapability::ImageDescription
        || request.prompt.is_some()
        || media_type != "image/png"
        || page == 0
    {
        return Err(ConversionError::Ai {
            provider: provider.into(),
            detail: "image-description request contract is invalid".into(),
        });
    }
    let expected = ProviderNetworkPolicy {
        allow_network: options.network.enabled,
        allow_private_network: !options.network.deny_private_networks,
        allowed_hosts: options.network.allowed_hosts.clone(),
    };
    if network != &expected {
        return Err(ConversionError::Network {
            detail: "AI provider network policy does not match this invocation".into(),
        });
    }
    Ok(())
}

fn map_provider_error(provider: &str, error: &ProviderError) -> ConversionError {
    match error.code() {
        ProviderErrorCode::Cancelled => ConversionError::Cancelled,
        ProviderErrorCode::Timeout => ConversionError::Timeout,
        ProviderErrorCode::ResourceLimit | ProviderErrorCode::ResponseTooLarge => {
            memory(error.code_str())
        }
        ProviderErrorCode::NetworkDenied
        | ProviderErrorCode::HostDenied
        | ProviderErrorCode::PrivateNetworkDenied
        | ProviderErrorCode::Dns
        | ProviderErrorCode::Connect
        | ProviderErrorCode::Tls
        | ProviderErrorCode::RedirectDenied => {
            ConversionError::Network { detail: error.code_str().into() }
        }
        _ => ConversionError::Ai { provider: provider.into(), detail: error.code_str().into() },
    }
}

fn memory(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
