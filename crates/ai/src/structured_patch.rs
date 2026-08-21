//! Policy-bound structured document repair and post-processing.

use crate::{
    GenerationEndpoint, GenerationInput, GenerationRequest, OpenAiCompatibleClient, ProviderError,
    ProviderErrorCode, ProviderNetworkPolicy,
};
use into_markdown_core::{
    AiCapability, AiInput, AiMode, AiOutput, AiProvider, AiRequest, Block, BlockNode, BoxFuture,
    ConversionError, ConversionOptions, ConverterOutput, Diagnostic, DiagnosticSeverity, Document,
    EnrichmentPlan, ExecutionContext, InputFormat, OutputEnricher, Services,
    estimate_retained_output,
};
use std::collections::BTreeSet;

const ENRICHER_ID: &str = "builtin.ai.structured-patch";
const MAX_PATCH_INPUT_BYTES: usize = 192 * 1024;
const MAX_PATCH_OUTPUT_TOKENS: u32 = 16_384;
const FIXED_WORKING_BYTES: u64 = 1024 * 1024;

/// OpenAI-compatible provider that accepts a validated Document IR snapshot and
/// returns only the versioned [`into_markdown_core::DocumentPatch`] schema.
pub struct OpenAiDocumentPatchProvider {
    client: OpenAiCompatibleClient,
    network: ProviderNetworkPolicy,
    provider_id: String,
    capabilities: BTreeSet<AiCapability>,
}

impl std::fmt::Debug for OpenAiDocumentPatchProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiDocumentPatchProvider")
            .field("client", &self.client)
            .field("network", &self.network)
            .field("provider_id", &self.provider_id)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl OpenAiDocumentPatchProvider {
    /// Bind a transport to an exact provider identity and the structured repair
    /// capabilities explicitly declared by configuration.
    ///
    /// # Errors
    ///
    /// Rejects invalid identities, empty capability sets, and capabilities
    /// whose output is not represented by a Document Patch.
    pub fn new(
        client: OpenAiCompatibleClient,
        network: ProviderNetworkPolicy,
        provider_id: impl Into<String>,
        capabilities: impl IntoIterator<Item = AiCapability>,
    ) -> Result<Self, ConversionError> {
        let provider_id = provider_id.into();
        validate_provider_id(&provider_id)?;
        let capabilities = capabilities.into_iter().collect::<BTreeSet<_>>();
        if capabilities.is_empty()
            || capabilities.iter().any(|capability| !is_patch_capability(*capability))
        {
            return Err(ConversionError::ComponentUnavailable {
                component: provider_id,
                detail: "structured patch provider capabilities are invalid".into(),
            });
        }
        Ok(Self { client, network, provider_id, capabilities })
    }
}

impl AiProvider for OpenAiDocumentPatchProvider {
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
        context.checkpoint()?;
        let document = validate_patch_request(request, options, &self.network, &self.capabilities)?;
        document.validate().map_err(|error| ConversionError::Ai {
            provider: self.provider_id.clone(),
            detail: format!("structured patch input is invalid at {}", error.path),
        })?;
        let retained = estimate_retained_output(document, &Vec::new(), &Vec::new())?;
        if retained > u64::try_from(MAX_PATCH_INPUT_BYTES).map_err(|_| memory_overflow())? {
            return Err(ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "structured patch input exceeds its bounded transport envelope".into(),
            });
        }
        let response = options.limits.max_field_bytes.min(256 * 1024);
        let plan = retained
            .checked_mul(8)
            .and_then(|value| value.checked_add(response))
            .and_then(|value| value.checked_add(FIXED_WORKING_BYTES))
            .ok_or_else(memory_overflow)?;
        if plan > options.limits.max_memory_bytes || plan > context.available_memory_bytes() {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "structured patch request exceeds the available memory budget".into(),
            });
        }
        Ok(plan)
    }

    fn execute_with_options<'a>(
        &'a self,
        request: AiRequest<'a>,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let document =
                validate_patch_request(request, options, &self.network, &self.capabilities)?;
            let document_json = document.to_json().map_err(|error| ConversionError::Ai {
                provider: self.provider_id.clone(),
                detail: format!("structured patch input is invalid at {}", error.path),
            })?;
            if document_json.len() > MAX_PATCH_INPUT_BYTES {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_input_bytes",
                    detail: "structured patch input exceeds 192 KiB".into(),
                });
            }
            let capability = capability_name(request.capability);
            let prompt = format!(
                "Return JSON only. Propose a version 1 DocumentPatch for {capability}. \
                 The exact schema is {{\"version\":1,\"operations\":[{{\"op\":\"append\",\"nodes\":[BlockNode]}},{{\"op\":\"replace\",\"target\":NodeId,\"nodes\":[BlockNode]}}]}}. \
                 Every returned node and nested node must use provenance.kind=\"aiProvider\" and provenance.provider=\"{}\". \
                 Do not return raw Markdown, commentary, diagnostics, unknown fields, or operations outside this schema. Document IR: {document_json}",
                self.provider_id
            );
            let result = self
                .client
                .generate(
                    GenerationRequest {
                        endpoint: GenerationEndpoint::ChatCompletions,
                        capability,
                        input: GenerationInput::Text(&prompt),
                        max_output_tokens: MAX_PATCH_OUTPUT_TOKENS,
                        idempotency_key: None,
                    },
                    context,
                )
                .map_err(|error| map_provider_error(&self.provider_id, &error))?;
            let patch =
                serde_json::from_str(result.text.trim()).map_err(|_| ConversionError::Ai {
                    provider: self.provider_id.clone(),
                    detail: "provider returned an invalid structured patch".into(),
                })?;
            Ok(AiOutput { patch: Some(patch), ..AiOutput::default() })
        })
    }

    fn execute<'a>(
        &'a self,
        _request: AiRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Err(ConversionError::ComponentUnavailable {
                component: self.provider_id.clone(),
                detail: "policy-bound structured patch execution is required".into(),
            })
        })
    }
}

/// Transactional engine stage for layout, table, formula, and Markdown repair.
/// Provider output is never copied directly into the final document: it must be
/// a single validated patch which is atomically applied to the current IR.
#[derive(Debug, Default)]
pub struct StructuredAiPatchEnricher;

impl OutputEnricher for StructuredAiPatchEnricher {
    fn id(&self) -> &'static str {
        ENRICHER_ID
    }

    fn planned_enrichment_bytes(
        &self,
        output: &ConverterOutput,
        _converter_id: &str,
        _format: InputFormat,
        options: &ConversionOptions,
        services: &Services,
        context: &ExecutionContext,
    ) -> Result<EnrichmentPlan, ConversionError> {
        let selected = selected_capabilities(output, options);
        if selected.is_empty() {
            return Ok(EnrichmentPlan::Skip);
        }
        let Some(provider) = services.ai.as_deref() else {
            return if selected.iter().any(|(_, mode)| *mode == AiMode::Only) {
                Err(ConversionError::ComponentUnavailable {
                    component: ENRICHER_ID.into(),
                    detail: "a required structured AI capability has no provider".into(),
                })
            } else {
                Ok(EnrichmentPlan::Skip)
            };
        };
        let advertised = provider.capabilities();
        let retained =
            estimate_retained_output(&output.document, &output.assets, &output.diagnostics)?;
        let mut plan = retained.checked_mul(2).ok_or_else(memory_overflow)?;
        let mut invoked = false;
        for (capability, mode) in selected {
            if !advertised.contains(&capability) {
                if mode == AiMode::Only {
                    return Err(ConversionError::ComponentUnavailable {
                        component: provider.id().into(),
                        detail: format!(
                            "provider does not declare {}",
                            capability_name(capability)
                        ),
                    });
                }
                continue;
            }
            let request =
                AiRequest { capability, input: AiInput::Document(&output.document), prompt: None };
            match provider.planned_output_bytes(request, options, context) {
                Ok(bytes) => {
                    plan = plan.checked_add(bytes).ok_or_else(memory_overflow)?;
                    invoked = true;
                }
                Err(error) if mode != AiMode::Only && recoverable_provider_error(&error) => {
                    return Ok(EnrichmentPlan::Skip);
                }
                Err(error) => return Err(error),
            }
        }
        if invoked { Ok(EnrichmentPlan::Reserve(plan)) } else { Ok(EnrichmentPlan::Skip) }
    }

    fn enrich<'a>(
        &'a self,
        mut output: ConverterOutput,
        _converter_id: &'a str,
        _format: InputFormat,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            let Some(provider) = services.ai.as_deref() else {
                return Ok(output);
            };
            let advertised = provider.capabilities();
            for (capability, mode) in selected_capabilities(&output, options) {
                context.checkpoint()?;
                if !advertised.contains(&capability) {
                    if mode == AiMode::Only {
                        return Err(ConversionError::ComponentUnavailable {
                            component: provider.id().into(),
                            detail: format!(
                                "provider does not declare {}",
                                capability_name(capability)
                            ),
                        });
                    }
                    continue;
                }
                let request = AiRequest {
                    capability,
                    input: AiInput::Document(&output.document),
                    prompt: None,
                };
                let response = provider.execute_with_options(request, options, context).await;
                let ai = match response {
                    Ok(ai) => ai,
                    Err(error) if mode != AiMode::Only && recoverable_provider_error(&error) => {
                        output.diagnostics.push(Diagnostic {
                            code: "aiCapabilityFallback".into(),
                            severity: DiagnosticSeverity::Warning,
                            message: format!(
                                "{} was unavailable; deterministic output was retained",
                                capability_name(capability)
                            ),
                            locator: None,
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if !ai.nodes.is_empty() || !ai.diagnostics.is_empty() {
                    return Err(ConversionError::Ai {
                        provider: provider.id().into(),
                        detail: "structured repair returned direct nodes or diagnostics".into(),
                    });
                }
                let patch = ai.patch.ok_or_else(|| ConversionError::Ai {
                    provider: provider.id().into(),
                    detail: "structured repair response omitted a DocumentPatch".into(),
                })?;
                let candidate = patch.apply(&output.document, provider.id())?;
                validate_patch_asset_references(
                    &candidate.blocks,
                    &output.assets.iter().map(|asset| asset.id.0.as_str()).collect::<BTreeSet<_>>(),
                    provider.id(),
                )?;
                output.document = candidate;
            }
            Ok(output)
        })
    }
}

fn validate_patch_asset_references(
    nodes: &[BlockNode],
    inventory: &BTreeSet<&str>,
    provider: &str,
) -> Result<(), ConversionError> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } if !inventory.contains(asset.0.as_str()) => {
                return Err(ConversionError::Ai {
                    provider: provider.into(),
                    detail: "structured patch references an unknown asset".into(),
                });
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                validate_patch_asset_references(blocks, inventory, provider)?;
            }
            Block::List { items, .. } => {
                for item in items {
                    validate_patch_asset_references(&item.blocks, inventory, provider)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        validate_patch_asset_references(&cell.blocks, inventory, provider)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn selected_capabilities(
    output: &ConverterOutput,
    options: &ConversionOptions,
) -> Vec<(AiCapability, AiMode)> {
    [
        (AiCapability::LayoutRepair, options.ai.layout_repair),
        (AiCapability::TableRepair, options.ai.table_repair),
        (AiCapability::FormulaRepair, options.ai.formula_repair),
        (AiCapability::MarkdownPostprocess, options.ai.markdown_postprocess),
    ]
    .into_iter()
    .filter(|(capability, mode)| {
        *mode != AiMode::Off && (*mode != AiMode::Fallback || needs_fallback(output, *capability))
    })
    .collect()
}

fn needs_fallback(output: &ConverterOutput, capability: AiCapability) -> bool {
    let needle = capability_name(capability).replace('-', "");
    output.diagnostics.iter().any(|diagnostic| {
        let code = diagnostic.code.to_ascii_lowercase().replace(['-', '_'], "");
        diagnostic.severity != DiagnosticSeverity::Info && code.contains(&needle)
    }) || match capability {
        AiCapability::LayoutRepair => contains_block(&output.document.blocks, |block| {
            matches!(block, Block::Page { .. } | Block::Slide { .. })
        }),
        AiCapability::TableRepair => {
            contains_block(&output.document.blocks, |block| matches!(block, Block::Table { .. }))
        }
        AiCapability::FormulaRepair => {
            contains_block(&output.document.blocks, |block| matches!(block, Block::Formula(_)))
        }
        AiCapability::MarkdownPostprocess => !output.diagnostics.is_empty(),
        _ => false,
    }
}

fn contains_block(nodes: &[BlockNode], predicate: impl Fn(&Block) -> bool + Copy) -> bool {
    nodes.iter().any(|node| {
        predicate(&node.block)
            || match &node.block {
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => contains_block(blocks, predicate),
                Block::List { items, .. } => {
                    items.iter().any(|item| contains_block(&item.blocks, predicate))
                }
                Block::Table { rows, .. } => rows.iter().any(|row| {
                    row.cells.iter().any(|cell| contains_block(&cell.blocks, predicate))
                }),
                _ => false,
            }
    })
}

fn validate_patch_request<'a>(
    request: AiRequest<'a>,
    options: &ConversionOptions,
    network: &ProviderNetworkPolicy,
    capabilities: &BTreeSet<AiCapability>,
) -> Result<&'a Document, ConversionError> {
    if request.prompt.is_some()
        || !capabilities.contains(&request.capability)
        || !is_patch_capability(request.capability)
    {
        return Err(ConversionError::Ai {
            provider: "structured-patch".into(),
            detail: "structured patch request contract is invalid".into(),
        });
    }
    let AiInput::Document(document) = request.input else {
        return Err(ConversionError::Ai {
            provider: "structured-patch".into(),
            detail: "structured patch requires Document IR input".into(),
        });
    };
    let expected = ProviderNetworkPolicy {
        allow_network: options.network.enabled,
        allow_private_network: !options.network.deny_private_networks,
        allowed_hosts: options.network.allowed_hosts.clone(),
    };
    if network != &expected {
        return Err(ConversionError::Network {
            detail: "structured patch network policy does not match this invocation".into(),
        });
    }
    Ok(document)
}

const fn is_patch_capability(capability: AiCapability) -> bool {
    matches!(
        capability,
        AiCapability::LayoutRepair
            | AiCapability::TableRepair
            | AiCapability::FormulaRepair
            | AiCapability::MarkdownPostprocess
    )
}

const fn capability_name(capability: AiCapability) -> &'static str {
    match capability {
        AiCapability::LayoutRepair => "layout-repair",
        AiCapability::TableRepair => "table-repair",
        AiCapability::FormulaRepair => "formula-repair",
        AiCapability::MarkdownPostprocess => "markdown-postprocess",
        AiCapability::VisionOcr => "vision-ocr",
        AiCapability::ImageDescription => "image-description",
        AiCapability::AudioTranscription => "audio-transcription",
    }
}

fn validate_provider_id(value: &str) -> Result<(), ConversionError> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_control)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(ConversionError::ComponentUnavailable {
            component: "structured-patch-provider".into(),
            detail: "provider identity is invalid".into(),
        });
    }
    Ok(())
}

fn recoverable_provider_error(error: &ConversionError) -> bool {
    matches!(
        error,
        ConversionError::ComponentUnavailable { .. }
            | ConversionError::Network { .. }
            | ConversionError::Timeout
            | ConversionError::Ai { .. }
    )
}

fn map_provider_error(provider: &str, error: &ProviderError) -> ConversionError {
    match error.code() {
        ProviderErrorCode::Cancelled => ConversionError::Cancelled,
        ProviderErrorCode::Timeout => ConversionError::Timeout,
        ProviderErrorCode::ResourceLimit | ProviderErrorCode::ResponseTooLarge => {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: error.code_str().into(),
            }
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

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "structured patch memory plan overflow".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        AssetId, Block, DocumentPatch, ExecutionOptions, Inline, NodeId, PatchOperation,
        Provenance, ProvenanceKind, ResourceLimits, SourceLocator,
    };
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct FixtureProvider {
        result: Result<AiOutput, ConversionError>,
        calls: AtomicUsize,
    }

    impl AiProvider for FixtureProvider {
        fn id(&self) -> &'static str {
            "fixture.remote"
        }

        fn capabilities(&self) -> BTreeSet<AiCapability> {
            BTreeSet::from([
                AiCapability::LayoutRepair,
                AiCapability::TableRepair,
                AiCapability::FormulaRepair,
                AiCapability::MarkdownPostprocess,
            ])
        }

        fn planned_output_bytes(
            &self,
            _request: AiRequest<'_>,
            _options: &ConversionOptions,
            _context: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(4_096)
        }

        fn execute_with_options<'a>(
            &'a self,
            _request: AiRequest<'a>,
            _options: &'a ConversionOptions,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn execute<'a>(
            &'a self,
            _request: AiRequest<'a>,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
            Box::pin(async {
                Err(ConversionError::Internal {
                    detail: "legacy execution must not be used".into(),
                })
            })
        }
    }

    fn node(id: &str, text: &str, provider: &str) -> BlockNode {
        BlockNode {
            id: NodeId(id.into()),
            block: Block::Paragraph(vec![Inline::Text { value: text.into(), marks: Vec::new() }]),
            provenance: Provenance {
                kind: if provider == "native" {
                    ProvenanceKind::NativeParser
                } else {
                    ProvenanceKind::AiProvider
                },
                provider: provider.into(),
                locator: SourceLocator::default(),
                confidence: None,
            },
        }
    }

    fn output() -> ConverterOutput {
        ConverterOutput::new(
            Document { blocks: vec![node("source", "before", "native")], ..Document::default() },
            Vec::new(),
            Vec::new(),
        )
    }

    fn patch_output() -> AiOutput {
        AiOutput {
            patch: Some(DocumentPatch {
                version: 1,
                operations: vec![PatchOperation::Replace {
                    target: NodeId("source".into()),
                    nodes: vec![node("replacement", "after", "fixture.remote")],
                }],
            }),
            ..AiOutput::default()
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(value) => return value,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn run(
        provider: Arc<FixtureProvider>,
        mode: AiMode,
        mut output: ConverterOutput,
    ) -> Result<ConverterOutput, ConversionError> {
        let mut options = ConversionOptions::default();
        options.ai.markdown_postprocess = mode;
        if mode == AiMode::Fallback {
            output.diagnostics.push(Diagnostic {
                code: "markdownPostprocessRequired".into(),
                severity: DiagnosticSeverity::Warning,
                message: "fixture".into(),
                locator: None,
            });
        }
        let services = Services { ai: Some(provider), ..Services::default() };
        let context = context();
        assert!(matches!(
            StructuredAiPatchEnricher.planned_enrichment_bytes(
                &output,
                "fixture",
                InputFormat::Text,
                &options,
                &services,
                &context,
            )?,
            EnrichmentPlan::Reserve(_)
        ));
        block_on(StructuredAiPatchEnricher.enrich(
            output,
            "fixture",
            InputFormat::Text,
            &options,
            &services,
            &context,
        ))
    }

    #[test]
    fn valid_patch_is_applied_for_prefer_and_fallback() {
        for mode in [AiMode::Prefer, AiMode::Fallback, AiMode::Only] {
            let provider = Arc::new(FixtureProvider {
                result: Ok(patch_output()),
                calls: AtomicUsize::new(0),
            });
            let result = run(Arc::clone(&provider), mode, output()).unwrap();
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.document.blocks[0].id.0, "replacement");
            assert_eq!(result.document.blocks[0].provenance.provider, "fixture.remote");
        }
    }

    #[test]
    fn off_and_unneeded_fallback_never_invoke_provider() {
        for mode in [AiMode::Off, AiMode::Fallback] {
            let provider = Arc::new(FixtureProvider {
                result: Ok(patch_output()),
                calls: AtomicUsize::new(0),
            });
            let mut options = ConversionOptions::default();
            options.ai.markdown_postprocess = mode;
            let services = Services { ai: Some(provider.clone()), ..Services::default() };
            let context = context();
            assert_eq!(
                StructuredAiPatchEnricher
                    .planned_enrichment_bytes(
                        &output(),
                        "fixture",
                        InputFormat::Text,
                        &options,
                        &services,
                        &context,
                    )
                    .unwrap(),
                EnrichmentPlan::Skip
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn prefer_and_fallback_recover_provider_failures_but_only_fails_closed() {
        for mode in [AiMode::Prefer, AiMode::Fallback] {
            let provider = Arc::new(FixtureProvider {
                result: Err(ConversionError::Network { detail: "fixture".into() }),
                calls: AtomicUsize::new(0),
            });
            let result = run(provider, mode, output()).unwrap();
            assert_eq!(result.document.blocks[0].id.0, "source");
            assert!(result.diagnostics.iter().any(|item| item.code == "aiCapabilityFallback"));
        }

        let provider = Arc::new(FixtureProvider {
            result: Err(ConversionError::Network { detail: "fixture".into() }),
            calls: AtomicUsize::new(0),
        });
        assert!(matches!(
            run(provider, AiMode::Only, output()),
            Err(ConversionError::Network { .. })
        ));
    }

    #[test]
    fn direct_nodes_and_fatal_failures_never_fall_back_into_final_ir() {
        let provider = Arc::new(FixtureProvider {
            result: Ok(AiOutput {
                nodes: vec![node("direct", "bypass", "fixture.remote")],
                ..AiOutput::default()
            }),
            calls: AtomicUsize::new(0),
        });
        assert!(matches!(run(provider, AiMode::Prefer, output()), Err(ConversionError::Ai { .. })));

        let provider = Arc::new(FixtureProvider {
            result: Err(ConversionError::Cancelled),
            calls: AtomicUsize::new(0),
        });
        assert!(matches!(run(provider, AiMode::Prefer, output()), Err(ConversionError::Cancelled)));
    }

    #[test]
    fn patch_cannot_create_a_dangling_asset_reference() {
        let mut image = node("image", "unused", "fixture.remote");
        image.block = Block::Image {
            asset: AssetId("provider-invented".into()),
            alt: Some("invented".into()),
        };
        let provider = Arc::new(FixtureProvider {
            result: Ok(AiOutput {
                patch: Some(DocumentPatch {
                    version: 1,
                    operations: vec![PatchOperation::Replace {
                        target: NodeId("source".into()),
                        nodes: vec![image],
                    }],
                }),
                ..AiOutput::default()
            }),
            calls: AtomicUsize::new(0),
        });
        assert!(matches!(run(provider, AiMode::Only, output()), Err(ConversionError::Ai { .. })));
    }

    #[test]
    fn wire_schema_rejects_unknown_fields_and_unknown_operations() {
        let unknown_field = r#"{"version":1,"operations":[],"rawMarkdown":"bypass"}"#;
        let unknown_operation = r#"{"version":1,"operations":[{"op":"remove","target":"source"}]}"#;
        assert!(serde_json::from_str::<DocumentPatch>(unknown_field).is_err());
        assert!(serde_json::from_str::<DocumentPatch>(unknown_operation).is_err());
    }
}
