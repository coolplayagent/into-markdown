//! Policy-bound OCR routing and evidence-preserving IR construction.

use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, Diagnostic, DiagnosticSeverity,
    ExecutionContext, Inline, NodeId, OcrEvidence, OcrEvidenceStage, OcrEvidenceStep, OcrPolicy,
    OcrRecognition, OcrRequest, OcrSourceRegion, Provenance, ProvenanceKind, Services,
    SourceLocator, SourcePoint,
};

const MERGE_PROVIDER: &str = "builtin.converter.image.ocr-merge";
const INSTALL_HINT: &str =
    "install local OCR models with `into-md models install pp-ocrv6-tiny-zh-en`";

pub(super) struct OcrContribution {
    pub(super) nodes: Vec<BlockNode>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) accepted_text: bool,
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
    if options.ocr.policy == OcrPolicy::Off {
        return Ok(empty());
    }
    validate_options(options)?;
    let Some(engine) = services.ocr.as_deref() else {
        return unavailable(options.ocr.policy, page, "no OCR engine is configured");
    };
    let request =
        OcrRequest { image, media_type: "image/png", languages: &["zh-Hans", "en", "zh-Hant"] };
    let recognition = match context.run(engine.recognize_bound(request, context)).await? {
        Ok(value) => value,
        Err(error) if options.ocr.policy == OcrPolicy::Auto => {
            return Ok(degraded(page, format!("OCR was unavailable ({error}); {INSTALL_HINT}")));
        }
        Err(error) => return Err(map_unavailable(engine.id(), error)),
    };
    let OcrRecognition::Bound(bound) = recognition else {
        return unavailable(
            options.ocr.policy,
            page,
            "the configured OCR engine returned legacy output without bound detector/model evidence",
        );
    };
    let (result, detection_confidences, mut chain) = bound.into_parts();
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

    let mut nodes = Vec::new();
    let mut diagnostics = Vec::new();
    for (index, (region, detection_confidence)) in
        result.regions.into_iter().zip(detection_confidences).enumerate()
    {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        validate_region(&region.polygon, width, height, engine.id())?;
        if !region.confidence.is_finite() || !(0.0..=1.0).contains(&region.confidence) {
            return Err(ConversionError::Ocr {
                provider: engine.id().into(),
                detail: "recognition confidence must be finite and between zero and one".into(),
            });
        }
        let confidence = region.confidence.min(detection_confidence);
        if region.text.trim().is_empty() || confidence < options.ocr.minimum_confidence {
            diagnostics.push(Diagnostic {
                code: "image.ocrLowConfidence".into(),
                severity: DiagnosticSeverity::Info,
                message: format!("OCR region {index} was omitted below the configured confidence"),
                locator: Some(page_locator(page, width, height, None)),
            });
            continue;
        }
        let polygon = region.polygon.map(|(x, y)| SourcePoint { x, y });
        let bounds = polygon_bounds(&polygon);
        let locator = page_locator(page, width, height, Some(bounds));
        let evidence = OcrEvidence {
            page,
            regions: vec![OcrSourceRegion {
                source_index: u32::try_from(index).map_err(|_| ConversionError::ResourceLimit {
                    limit: "documentInlines",
                    detail: "OCR region index exceeds u32".into(),
                })?,
                polygon,
                detection_confidence,
                recognition_confidence: region.confidence,
            }],
            chain: chain.clone(),
        };
        let provenance = Provenance {
            kind: ProvenanceKind::LocalOcr,
            provider: result.provider.clone(),
            locator,
            confidence: Some(confidence),
        };
        nodes.push(BlockNode {
            id: NodeId(format!("image-page-{page}-ocr-{}", index + 1)),
            block: Block::Paragraph(vec![Inline::OcrText {
                value: region.text,
                marks: vec![],
                provenance: Box::new(provenance.clone()),
                evidence: Box::new(evidence),
            }]),
            provenance,
        });
    }
    Ok(OcrContribution { accepted_text: !nodes.is_empty(), nodes, diagnostics })
}

fn validate_region(
    polygon: &[(f32, f32); 4],
    width: u32,
    height: u32,
    provider: &str,
) -> Result<(), ConversionError> {
    let width = dimension(width, provider)?;
    let height = dimension(height, provider)?;
    if polygon.iter().any(|(x, y)| {
        !x.is_finite() || !y.is_finite() || *x < 0.0 || *y < 0.0 || *x > width || *y > height
    }) {
        return Err(ConversionError::Ocr {
            provider: provider.into(),
            detail: "OCR polygon lies outside the normalized image".into(),
        });
    }
    let mut sign = 0_i8;
    for index in 0..4 {
        let a = polygon[index];
        let b = polygon[(index + 1) % 4];
        let c = polygon[(index + 2) % 4];
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if cross == 0.0 {
            return Err(ConversionError::Ocr {
                provider: provider.into(),
                detail: "OCR polygon must be a non-degenerate convex quadrilateral".into(),
            });
        }
        let current = if cross > 0.0 { 1 } else { -1 };
        if sign != 0 && sign != current {
            return Err(ConversionError::Ocr {
                provider: provider.into(),
                detail: "OCR polygon must be convex and consistently ordered".into(),
            });
        }
        sign = current;
    }
    Ok(())
}

fn polygon_bounds(points: &[SourcePoint; 4]) -> into_markdown_core::Rect {
    let min_x = points.iter().map(|point| point.x).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|point| point.x).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|point| point.y).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|point| point.y).fold(f32::NEG_INFINITY, f32::max);
    into_markdown_core::Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y }
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
    }
}

fn map_unavailable(provider: &str, error: ConversionError) -> ConversionError {
    match error {
        ConversionError::Cancelled
        | ConversionError::Timeout
        | ConversionError::ResourceLimit { .. } => error,
        _ => ConversionError::ComponentUnavailable {
            component: provider.into(),
            detail: format!("{error}; {INSTALL_HINT}"),
        },
    }
}

fn empty() -> OcrContribution {
    OcrContribution { nodes: vec![], diagnostics: vec![], accepted_text: false }
}
