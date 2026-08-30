//! Policy-bound OCR routing and evidence-preserving IR construction.

use into_markdown_core::{
    AiMode, Asset, Block, BlockNode, ConversionError, ConversionOptions, Diagnostic,
    DiagnosticSeverity, Document, ExecutionContext, Inline, NodeId, OcrEvidence, OcrEvidenceStage,
    OcrEvidenceStep, OcrInputIdentity, OcrOutputPlan, OcrPolicy, OcrRecognition, OcrRequest,
    OcrResult, OcrSourceRegion, Provenance, ProvenanceKind, ResourceReservation, Services,
    SourceLocator, SourcePoint, estimate_retained_output,
};

const MERGE_PROVIDER: &str = "builtin.converter.image.ocr-merge";
const INSTALL_HINT: &str = "install the local OCR capability with `into-md setup ocr`";

mod geometry;
use geometry::{polygon_bounds, validate_region_bounds, validate_region_shape};

#[derive(Debug)]
pub(crate) struct OcrContribution {
    pub(crate) nodes: Vec<BlockNode>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(super) accepted_text: bool,
    pub(crate) memory: Option<ResourceReservation>,
    pub(crate) recognized_regions: u64,
    pub(crate) recognized_chars: u64,
}

pub(super) async fn recognize(
    image: &[u8],
    page: u32,
    width: u32,
    height: u32,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<OcrContribution, ConversionError> {
    recognize_inner(image, page, width, height, None, options, services, context).await
}

#[allow(clippy::too_many_arguments)] // Source identity is intentionally explicit at this boundary.
pub(crate) async fn recognize_for_input(
    image: &[u8],
    page: u32,
    width: u32,
    height: u32,
    identity: OcrInputIdentity,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<OcrContribution, ConversionError> {
    recognize_inner(image, page, width, height, Some(identity), options, services, context).await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Shared adapter retains the public OCR request fields.
async fn recognize_inner(
    image: &[u8],
    page: u32,
    width: u32,
    height: u32,
    expected_identity: Option<OcrInputIdentity>,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<OcrContribution, ConversionError> {
    let policy = effective_ocr_policy(options);
    if policy == OcrPolicy::Off {
        return Ok(empty());
    }
    context.checkpoint()?;
    validate_options(options)?;
    let Some(engine) = services.ocr.as_deref() else {
        return unavailable(policy, page, "no OCR engine is configured");
    };
    let request =
        OcrRequest { image, media_type: "image/png", languages: &["zh-Hans", "en", "zh-Hant"] };
    let plan = match engine.planned_bound_output(request, options, context) {
        Ok(plan) => plan,
        Err(error) => return preflight_unavailable(policy, page, engine.id(), error),
    };
    validate_plan(plan, options, context)?;
    let planned_bytes = plan
        .max_retained_bytes()
        .checked_add(plan.max_working_bytes())
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "OCR provider memory plan overflow".into(),
        })?;
    // An enclosing converter/enricher credit already reserved the complete
    // provider peak. In that case retain only the output allowance here and
    // let the provider charge its real working set to the enclosing credit.
    // Reserving the working allowance again would double-charge that peak.
    let reservation_bytes =
        if context.has_memory_credit() { plan.max_retained_bytes() } else { planned_bytes };
    let mut memory = context.reserve_memory(reservation_bytes)?;
    let credited = (!context.has_memory_credit())
        .then(|| context.with_memory_credit(&mut memory))
        .transpose()?;
    let provider_context = credited.as_deref().unwrap_or(context);
    let recognition = match context.run(engine.recognize_bound(request, provider_context)).await? {
        Ok(value) => value,
        Err(error @ ConversionError::ComponentUnavailable { .. }) if policy == OcrPolicy::Auto => {
            return Ok(degraded(page, format!("OCR was unavailable ({error})")));
        }
        Err(error) if policy == OcrPolicy::Auto => return Err(error),
        Err(error) => return Err(map_unavailable(engine.id(), error)),
    };
    drop(credited);
    let bound = match recognition {
        OcrRecognition::Bound(bound) => bound,
        OcrRecognition::Remote(result) => {
            let (document, diagnostics, recognized_regions, recognized_chars) =
                materialize_remote_unbound(result, page, plan, options, context)?;
            let assets = Vec::<Asset>::new();
            let retained = estimate_retained_output(&document, &assets, &diagnostics)?;
            if retained > plan.max_retained_bytes() {
                return Err(ConversionError::Ocr {
                    provider: engine.id().into(),
                    detail: format!("remote OCR retained {retained} bytes beyond its plan"),
                });
            }
            if retained < reservation_bytes {
                memory.shrink(reservation_bytes - retained)?;
            }
            return Ok(OcrContribution {
                accepted_text: !document.blocks.is_empty(),
                nodes: document.blocks,
                diagnostics,
                memory: Some(memory),
                recognized_regions,
                recognized_chars,
            });
        }
        OcrRecognition::Unbound(_) => {
            return unavailable(
                policy,
                page,
                "the configured local OCR engine returned legacy output without bound detector/model evidence",
            );
        }
        _ => {
            return Err(ConversionError::Ocr {
                provider: engine.id().into(),
                detail: "OCR provider returned an unsupported recognition contract".into(),
            });
        }
    };
    if let Some(expected) = expected_identity
        && bound.input_identity() != Some(expected)
    {
        return Err(ConversionError::Ocr {
            provider: engine.id().into(),
            detail: "bound OCR result does not match the normalized source image".into(),
        });
    }
    let (result, detection_confidences, mut chain) = bound.into_parts();
    validate_bound_payload(
        &result,
        detection_confidences.capacity(),
        &chain,
        chain.capacity(),
        plan,
        options,
        engine.id(),
    )?;
    if result.provider.trim().is_empty() || chain[1].provider != result.provider {
        return Err(ConversionError::Ocr {
            provider: engine.id().into(),
            detail: "bound OCR provider identity is inconsistent".into(),
        });
    }
    chain.push(OcrEvidenceStep {
        stage: OcrEvidenceStage::Merge,
        provider: MERGE_PROVIDER.into(),
        model: None,
    });
    let materialize = MaterializeContext {
        chain: &chain,
        page,
        width,
        height,
        options,
        engine_id: engine.id(),
        execution: context,
    };
    let (document, diagnostics, recognized_regions, recognized_chars) =
        materialize_nodes(result, detection_confidences, &materialize)?;
    let assets = Vec::<Asset>::new();
    let retained = estimate_retained_output(&document, &assets, &diagnostics)?;
    if retained > plan.max_retained_bytes() {
        return Err(ConversionError::Ocr {
            provider: engine.id().into(),
            detail: format!("structured OCR retained {retained} bytes beyond its plan"),
        });
    }
    if retained < reservation_bytes {
        memory.shrink(reservation_bytes - retained)?;
    }
    Ok(OcrContribution {
        accepted_text: !document.blocks.is_empty(),
        nodes: document.blocks,
        diagnostics,
        memory: Some(memory),
        recognized_regions,
        recognized_chars,
    })
}

fn effective_ocr_policy(options: &ConversionOptions) -> OcrPolicy {
    match options.ai.vision_ocr {
        AiMode::Only => OcrPolicy::Always,
        AiMode::Fallback | AiMode::Prefer if options.ocr.policy == OcrPolicy::Off => {
            OcrPolicy::Auto
        }
        _ => options.ocr.policy,
    }
}

fn materialize_remote_unbound(
    result: OcrResult,
    page: u32,
    plan: OcrOutputPlan,
    options: &ConversionOptions,
    execution: &ExecutionContext,
) -> Result<(Document, Vec<Diagnostic>, u64, u64), ConversionError> {
    let provider = result.provider.clone();
    if provider.trim().is_empty()
        || result.regions.len() > usize::try_from(plan.max_regions()).unwrap_or(usize::MAX)
    {
        return Err(ConversionError::Ocr {
            provider,
            detail: "remote OCR provider identity or region count is invalid".into(),
        });
    }
    let mut total_text = 0_u64;
    let mut recognized_regions = 0_u64;
    let mut recognized_chars = 0_u64;
    let mut nodes = Vec::new();
    for (index, region) in result.regions.into_iter().enumerate() {
        if index % 64 == 0 {
            execution.checkpoint()?;
        }
        let text = region.text.trim();
        if text.is_empty() {
            continue;
        }
        total_text = total_text
            .checked_add(u64::try_from(text.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: "remote OCR text size is unrepresentable".into(),
            })?)
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: "remote OCR text size overflow".into(),
            })?;
        if total_text > plan.max_text_bytes() || total_text > options.limits.max_field_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: "remote OCR text exceeds the configured output bound".into(),
            });
        }
        recognized_regions = recognized_regions.checked_add(1).ok_or_else(payload_limit)?;
        recognized_chars = recognized_chars
            .checked_add(u64::try_from(text.chars().count()).map_err(|_| payload_limit())?)
            .ok_or_else(payload_limit)?;
        nodes.push(BlockNode {
            id: NodeId(format!("remote-ocr-page-{page}-{index}")),
            block: Block::Paragraph(vec![Inline::Text { value: text.into(), marks: Vec::new() }]),
            provenance: Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: provider.clone(),
                locator: SourceLocator { page: Some(page), ..SourceLocator::default() },
                confidence: None,
            },
        });
    }
    if nodes.is_empty() {
        return Err(ConversionError::Ocr {
            provider,
            detail: "remote OCR returned no usable text".into(),
        });
    }
    let document = Document { blocks: nodes, ..Document::default() };
    document.validate().map_err(|error| ConversionError::Ocr {
        provider,
        detail: format!("remote OCR returned invalid IR at {}: {}", error.path, error.detail),
    })?;
    Ok((document, Vec::new(), recognized_regions, recognized_chars))
}

struct MaterializeContext<'a> {
    chain: &'a [OcrEvidenceStep],
    page: u32,
    width: u32,
    height: u32,
    options: &'a ConversionOptions,
    engine_id: &'a str,
    execution: &'a ExecutionContext,
}

fn materialize_nodes(
    result: OcrResult,
    detection_confidences: Vec<f32>,
    materialize: &MaterializeContext<'_>,
) -> Result<(Document, Vec<Diagnostic>, u64, u64), ConversionError> {
    let mut nodes = Vec::new();
    let mut diagnostics = Vec::new();
    let mut recognized_regions = 0_u64;
    let mut recognized_chars = 0_u64;
    for (index, (region, detection_confidence)) in
        result.regions.into_iter().zip(detection_confidences).enumerate()
    {
        if index % 256 == 0 {
            materialize.execution.checkpoint()?;
        }
        validate_region_bounds(&region.polygon, materialize)?;
        if !region.confidence.is_finite() || !(0.0..=1.0).contains(&region.confidence) {
            return Err(ConversionError::Ocr {
                provider: materialize.engine_id.into(),
                detail: "recognition confidence must be finite and between zero and one".into(),
            });
        }
        let confidence = region.confidence.min(detection_confidence);
        if region.text.trim().is_empty() || confidence < materialize.options.ocr.minimum_confidence
        {
            diagnostics.push(Diagnostic {
                code: "ocr.lowConfidence".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("OCR region {index} was omitted below the configured confidence"),
                locator: Some(page_locator(
                    materialize.page,
                    materialize.width,
                    materialize.height,
                    None,
                )),
            });
            continue;
        }
        // Empty/low-confidence detector noise is already diagnosed above.
        // Strict shape validation applies to evidence we actually publish.
        validate_region_shape(&region.polygon, materialize.engine_id)?;
        let polygon = region.polygon.map(|(x, y)| SourcePoint { x, y });
        let bounds = polygon_bounds(&polygon);
        let locator =
            page_locator(materialize.page, materialize.width, materialize.height, Some(bounds));
        let evidence = OcrEvidence {
            page: materialize.page,
            regions: vec![OcrSourceRegion {
                source_index: u32::try_from(index).map_err(|_| ConversionError::ResourceLimit {
                    limit: "documentInlines",
                    detail: "OCR region index exceeds u32".into(),
                })?,
                polygon,
                detection_confidence,
                recognition_confidence: region.confidence,
            }],
            chain: materialize.chain.to_vec(),
        };
        let provenance = Provenance {
            kind: ProvenanceKind::LocalOcr,
            provider: result.provider.clone(),
            locator,
            confidence: Some(confidence),
        };
        recognized_regions = recognized_regions.checked_add(1).ok_or_else(payload_limit)?;
        recognized_chars = recognized_chars
            .checked_add(u64::try_from(region.text.chars().count()).map_err(|_| payload_limit())?)
            .ok_or_else(payload_limit)?;
        nodes.push(BlockNode {
            id: NodeId(format!("image-page-{}-ocr-{}", materialize.page, index + 1)),
            block: Block::Paragraph(vec![Inline::OcrText {
                value: region.text,
                marks: vec![],
                provenance: Box::new(provenance.clone()),
                evidence: Box::new(evidence),
            }]),
            provenance,
        });
    }
    let document = Document { blocks: nodes, ..Document::default() };
    Ok((document, diagnostics, recognized_regions, recognized_chars))
}

fn validate_plan(
    plan: OcrOutputPlan,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if plan.max_regions() > options.limits.max_archive_entries {
        return Err(ConversionError::ResourceLimit {
            limit: "max_archive_entries",
            detail: "OCR output plan exceeds the request region limit".into(),
        });
    }
    if plan.max_text_bytes() > options.limits.max_field_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: "OCR output plan exceeds the request text limit".into(),
        });
    }
    let total =
        plan.max_retained_bytes().checked_add(plan.max_working_bytes()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "OCR provider memory plan overflow".into(),
            }
        })?;
    if total > options.limits.max_memory_bytes || total > context.available_memory_bytes() {
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "OCR output plan exceeds the request memory limit".into(),
        });
    }
    Ok(())
}

fn validate_bound_payload(
    result: &into_markdown_core::OcrResult,
    confidence_capacity: usize,
    chain: &[OcrEvidenceStep],
    chain_capacity: usize,
    plan: OcrOutputPlan,
    options: &ConversionOptions,
    provider: &str,
) -> Result<(), ConversionError> {
    let regions = u32::try_from(result.regions.len()).map_err(|_| payload_limit())?;
    let text_bytes = result.regions.iter().try_fold(0_u64, |total, region| {
        let length = u64::try_from(region.text.len()).map_err(|_| payload_limit())?;
        if length > options.limits.max_field_bytes {
            return Err(payload_limit());
        }
        total.checked_add(length).ok_or_else(payload_limit)
    })?;
    if regions > plan.max_regions() || regions > options.limits.max_archive_entries {
        return Err(ConversionError::ResourceLimit {
            limit: "max_archive_entries",
            detail: "bound OCR result exceeds its region plan".into(),
        });
    }
    if text_bytes > plan.max_text_bytes() || text_bytes > options.limits.max_field_bytes {
        return Err(payload_limit());
    }
    let confidence_bytes =
        confidence_capacity.checked_mul(std::mem::size_of::<f32>()).ok_or_else(payload_limit)?;
    let chain_bytes = chain_capacity
        .checked_mul(std::mem::size_of::<OcrEvidenceStep>())
        .ok_or_else(payload_limit)?;
    let capacities = result
        .regions
        .capacity()
        .checked_mul(std::mem::size_of::<into_markdown_core::OcrRegion>())
        .and_then(|bytes| bytes.checked_add(confidence_bytes))
        .and_then(|bytes| bytes.checked_add(chain_bytes))
        .and_then(|bytes| bytes.checked_add(result.provider.capacity()))
        .and_then(|bytes| {
            result
                .regions
                .iter()
                .try_fold(bytes, |total, region| total.checked_add(region.text.capacity()))
        })
        .and_then(|bytes| {
            chain.iter().try_fold(bytes, |total, step| {
                total.checked_add(step.provider.capacity()).and_then(|value| {
                    value.checked_add(step.model.as_ref().map_or(0, String::capacity))
                })
            })
        })
        .ok_or_else(payload_limit)?;
    if u64::try_from(capacities).map_err(|_| payload_limit())? > plan.max_retained_bytes() {
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("bound OCR provider {provider} retained more memory than its plan"),
        });
    }
    Ok(())
}

fn payload_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_field_bytes",
        detail: "bound OCR result exceeds its region or text plan".into(),
    }
}

fn preflight_unavailable(
    policy: OcrPolicy,
    page: u32,
    provider: &str,
    error: ConversionError,
) -> Result<OcrContribution, ConversionError> {
    match error {
        error @ ConversionError::ComponentUnavailable { .. } if policy == OcrPolicy::Auto => {
            Ok(degraded(page, format!("OCR was unavailable ({error})")))
        }
        error if policy == OcrPolicy::Auto => Err(error),
        ConversionError::Cancelled
        | ConversionError::Timeout
        | ConversionError::ResourceLimit { .. } => Err(error),
        _ => Err(map_unavailable(provider, error)),
    }
}

fn page_locator(
    page: u32,
    width: u32,
    height: u32,
    bounds: Option<into_markdown_core::Rect>,
) -> SourceLocator {
    SourceLocator {
        page: Some(page),
        bounds,
        page_width: Some(f32::from(u16::try_from(width).unwrap_or(u16::MAX))),
        page_height: Some(f32::from(u16::try_from(height).unwrap_or(u16::MAX))),
        ..SourceLocator::default()
    }
}

fn validate_options(options: &ConversionOptions) -> Result<(), ConversionError> {
    if options.ocr.minimum_confidence.is_finite()
        && (0.0..=1.0).contains(&options.ocr.minimum_confidence)
    {
        return Ok(());
    }
    Err(ConversionError::Ocr {
        provider: "options.ocr".into(),
        detail: "minimum_confidence must be finite and between zero and one".into(),
    })
}

fn dimension(value: u32, provider: &str) -> Result<f32, ConversionError> {
    u16::try_from(value).map(f32::from).map_err(|_| ConversionError::Ocr {
        provider: provider.into(),
        detail: "normalized image dimension exceeds the OCR coordinate range".into(),
    })
}

fn unavailable(
    policy: OcrPolicy,
    page: u32,
    detail: &str,
) -> Result<OcrContribution, ConversionError> {
    if policy == OcrPolicy::Auto {
        Ok(degraded(page, format!("{detail}; {INSTALL_HINT}")))
    } else {
        Err(ConversionError::ComponentUnavailable {
            component: "ocr.pp-ocrv6".into(),
            detail: format!("{detail}; {INSTALL_HINT}"),
        })
    }
}

fn degraded(page: u32, message: String) -> OcrContribution {
    OcrContribution {
        nodes: vec![],
        accepted_text: false,
        diagnostics: vec![Diagnostic {
            code: "image.ocrUnavailable".into(),
            severity: DiagnosticSeverity::Warning,
            message,
            locator: Some(SourceLocator { page: Some(page), ..SourceLocator::default() }),
        }],
        memory: None,
        recognized_regions: 0,
        recognized_chars: 0,
    }
}

fn map_unavailable(provider: &str, error: ConversionError) -> ConversionError {
    match error {
        ConversionError::Cancelled
        | ConversionError::Timeout
        | ConversionError::Ocr { .. }
        | ConversionError::ResourceLimit { .. } => error,
        _ => ConversionError::ComponentUnavailable {
            component: provider.into(),
            detail: error.to_string(),
        },
    }
}

fn empty() -> OcrContribution {
    OcrContribution {
        nodes: vec![],
        diagnostics: vec![],
        accepted_text: false,
        memory: None,
        recognized_regions: 0,
        recognized_chars: 0,
    }
}
