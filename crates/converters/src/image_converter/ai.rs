//! Transactional, capability-negotiated image-description routing.

use into_markdown_core::{
    AiCapability, AiInput, AiRequest, Asset, Block, BlockNode, ConversionError, ConversionOptions,
    Diagnostic, Document, ExecutionContext, Inline, ProvenanceKind, ResourceReservation, Services,
    estimate_retained_output,
};

pub(super) struct AiContribution {
    pub(super) nodes: Vec<BlockNode>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) memory: ResourceReservation,
}

pub(super) async fn describe(
    image: &[u8],
    page: u32,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<AiContribution, ConversionError> {
    let provider =
        services.ai.as_deref().ok_or_else(|| unavailable("no AI provider is configured"))?;
    if !provider.capabilities().contains(&AiCapability::ImageDescription) {
        return Err(unavailable("configured AI provider lacks ImageDescription capability"));
    }
    let request = AiRequest {
        capability: AiCapability::ImageDescription,
        input: AiInput::Image { bytes: image, media_type: "image/png" },
        prompt: None,
    };
    let plan = provider.planned_output_bytes(request, options, context)?;
    if plan > options.limits.max_memory_bytes || plan > context.available_memory_bytes() {
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("AI provider plan {plan} exceeds the remaining request budget"),
        });
    }
    let mut memory = context.reserve_memory(plan)?;
    let credited = (plan != 0).then(|| context.with_memory_credit(&mut memory)).transpose()?;
    let provider_context = credited.as_deref().unwrap_or(context);
    let output =
        match context.run(provider.execute_with_options(request, options, provider_context)).await?
        {
            Ok(output) => output,
            Err(error) => return Err(error),
        };
    drop(credited);
    context.checkpoint()?;
    if output.patch.is_some() {
        return Err(ConversionError::Ai {
            provider: provider.id().into(),
            detail: "image descriptions must return validated nodes, not document patches".into(),
        });
    }
    if output.nodes.is_empty() {
        return Err(ConversionError::Ai {
            provider: provider.id().into(),
            detail: "image-description provider returned no description nodes".into(),
        });
    }

    for node in &output.nodes {
        validate_node(node, page, provider.id())?;
    }
    for diagnostic in &output.diagnostics {
        if diagnostic.locator.as_ref().and_then(|locator| locator.page) != Some(page) {
            return Err(ConversionError::Ai {
                provider: provider.id().into(),
                detail: "AI diagnostic is not bound to the requested image page".into(),
            });
        }
    }
    let mut output = output;
    let document = Document { blocks: std::mem::take(&mut output.nodes), ..Document::default() };
    document.validate().map_err(|error| ConversionError::Ai {
        provider: provider.id().into(),
        detail: format!("AI returned invalid IR at {}: {}", error.path, error.detail),
    })?;
    let no_assets = Vec::<Asset>::new();
    let retained = estimate_retained_output(&document, &no_assets, &output.diagnostics)?;
    if retained > plan {
        return Err(ConversionError::Ai {
            provider: provider.id().into(),
            detail: format!("AI returned {retained} retained bytes beyond its {plan}-byte plan"),
        });
    }
    if retained < plan {
        memory.shrink(plan - retained)?;
    }
    Ok(AiContribution { nodes: document.blocks, diagnostics: output.diagnostics, memory })
}

fn validate_node(node: &BlockNode, page: u32, provider: &str) -> Result<(), ConversionError> {
    if node.provenance.kind != ProvenanceKind::AiProvider
        || node.provenance.provider != provider
        || node.provenance.locator.page != Some(page)
    {
        return Err(invalid(provider, "AI node provenance is not bound to its provider and page"));
    }
    match &node.block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            validate_inlines(inlines, provider)?;
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &item.blocks {
                    validate_node(child, page, provider)?;
                }
            }
        }
        Block::Code { .. } | Block::Formula(_) | Block::Rule => {}
        _ => {
            return Err(invalid(
                provider,
                "AI image description returned an unsupported structural node or resource",
            ));
        }
    }
    Ok(())
}

fn validate_inlines(inlines: &[Inline], provider: &str) -> Result<(), ConversionError> {
    for inline in inlines {
        match inline {
            Inline::Text { .. } | Inline::Code(_) | Inline::Formula(_) | Inline::LineBreak => {}
            Inline::Link { content, .. } => validate_inlines(content, provider)?,
            _ => {
                return Err(invalid(
                    provider,
                    "AI image description attempted to fabricate source or OCR identity",
                ));
            }
        }
    }
    Ok(())
}

fn invalid(provider: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Ai { provider: provider.into(), detail: detail.into() }
}

fn unavailable(detail: impl Into<String>) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "ai.image-description".into(),
        detail: detail.into(),
    }
}
