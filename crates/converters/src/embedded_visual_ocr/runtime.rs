//! Recognition is transactional per byte identity; only controlled worker refusals are optional.

use super::*;
use into_markdown_core::{
    ErrorPolicy, Inline, ProvenanceKind, ResourceFailureScope, ResourceLimitSource,
    ResourceRecoveryAction, ResourceRecoveryBoundary, ResourceUnitKind, classify_resource_recovery,
    recovery_diagnostic,
};

pub(super) fn require_scanned_pages(
    references: &mut [VisualRef],
    diagnostics: &[Diagnostic],
    format: InputFormat,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if format == InputFormat::Pdf {
        for diagnostic in diagnostics {
            context.checkpoint()?;
            if diagnostic.code == "pdf.scannedPage"
                && let Some(page) = diagnostic.locator.as_ref().and_then(|locator| locator.page)
            {
                // Use the parser's coverage/native-text classification. A page
                // number or short footer cannot replace the scanned body.
                for reference in references.iter_mut() {
                    context.checkpoint()?;
                    if reference.provenance.locator.page == Some(page) {
                        reference.optional = false;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn locate_omission(diagnostic: &mut Diagnostic, reference_index: usize) {
    if matches!(
        diagnostic.code.as_str(),
        "ocr.optionalRecognitionMemorySkipped" | "ocr.optionalRecognitionResourceSkipped"
    ) || (diagnostic.code.starts_with("resource.") && diagnostic.code.ends_with(".unitOmitted"))
    {
        if let Some(locator) = &mut diagnostic.locator
            && locator.part.is_none()
        {
            // A logical image reference identifies HTML nodes whose parser cannot
            // supply a source part or byte span. Preserve all available page geometry.
            locator.part = Some(format!("document/image/{}", reference_index + 1));
        }
    }
}

pub(super) fn has_native_body(
    nodes: &[BlockNode],
    image: &Provenance,
    format: InputFormat,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    for node in nodes {
        context.checkpoint()?;
        let origin = &node.provenance;
        if origin.kind != ProvenanceKind::NativeParser
            || (origin.provider != image.provider
                && !(format == InputFormat::Pdf
                    && origin.provider == "builtin.pdf.layout"
                    && image.provider == "builtin.converter.pdfium"))
            || !same_content_unit(&origin.locator, &image.locator, format)
            || origin.locator.page != image.locator.page
            || origin.locator.slide != image.locator.slide
            || origin.locator.sheet != image.locator.sheet
        {
            continue;
        }
        // Paragraphs and code carry body text. Container labels, headings,
        // image alt text and successful OCR from another image cannot authorize an omission.
        let body = match &node.block {
            Block::Paragraph(inlines) => native_inlines(inlines),
            Block::Code { text, .. } => has_body_characters(text),
            _ => false,
        };
        if body {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn optional_failure_code(
    error: &ConversionError,
    options: &ConversionOptions,
    _references: &[VisualRef],
    _asset_ids: impl Iterator<Item = AssetId>,
) -> Option<&'static str> {
    let code = match error {
        ConversionError::OcrRecognitionMemory { .. }
        | ConversionError::ResourceLimit {
            limit:
                "max_memory_bytes"
                | "ocrRecognitionMemory"
                | "recognitionMemory"
                | "recognitionCropMemory"
                | "recognitionOutputMemory",
            ..
        } => "ocr.optionalRecognitionMemorySkipped",
        ConversionError::ResourceLimit {
            limit:
                "recognitionWidth"
                | "recognitionCropPixels"
                | "recognitionTensorElements"
                | "recognitionOutputElements"
                | "recognitionRegions"
                | "recognitionDecodedBytes"
                | "ocrWidthLimit"
                | "ocrPixelLimit"
                | "ocrTensorLimit"
                | "ocrStructureLimit",
            ..
        } => "ocr.optionalRecognitionResourceSkipped",
        _ => return None,
    };
    let locator = SourceLocator::default();
    (classify_resource_recovery(
        options.error_policy,
        error,
        ResourceRecoveryBoundary {
            scope: ResourceFailureScope::VisualRecognition,
            unit: ResourceUnitKind::Image,
            locator: Some(&locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: 0,
            omitted_units: 1,
            limit_source: ResourceLimitSource::Explicit,
            precise_required: None,
            raised_limit: None,
        },
    ) == ResourceRecoveryAction::OmitUnit)
        .then_some(code)
}

pub(super) fn generic_omission_diagnostic(
    error: &ConversionError,
    options: &ConversionOptions,
) -> Option<Diagnostic> {
    let locator = SourceLocator::default();
    let facts = ResourceRecoveryBoundary {
        scope: ResourceFailureScope::VisualRecognition,
        unit: ResourceUnitKind::Image,
        locator: Some(&locator),
        rollback_complete: true,
        fallback_retained: true,
        committed_units: 0,
        omitted_units: 1,
        limit_source: ResourceLimitSource::Explicit,
        precise_required: None,
        raised_limit: None,
    };
    let action = classify_resource_recovery(options.error_policy, error, facts);
    let configured = match error.limit().map(|(limit, _)| limit) {
        Some("max_memory_bytes") => Some(options.limits.max_memory_bytes),
        Some("max_asset_bytes") => Some(options.limits.max_asset_bytes),
        Some("max_total_asset_bytes") => Some(options.limits.max_total_asset_bytes),
        Some("max_pages") => Some(u64::from(options.limits.max_pages)),
        _ => None,
    };
    recovery_diagnostic(error, action, facts, configured)
}

pub(super) fn omitted_contribution(
    error: &ConversionError,
    code: &'static str,
    options: &ConversionOptions,
) -> Result<super::CachedContribution, ConversionError> {
    let generic =
        generic_omission_diagnostic(error, options).ok_or_else(|| ConversionError::Internal {
            detail: "OCR omission did not produce a resource diagnostic".into(),
        })?;
    Ok(super::CachedContribution {
        diagnostics: vec![
            Diagnostic {
                code: code.into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("OCR was omitted and the original visual was retained: {error}"),
                locator: None,
            },
            generic,
        ],
        ..Default::default()
    })
}

pub(super) async fn recognize(
    asset: &into_markdown_core::Asset,
    ordinal: usize,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<(CachedContribution, Option<ResourceReservation>), ConversionError> {
    let normalized = match normalize(asset, options, context) {
        Ok(value) => value,
        Err(error)
            if effective_ocr_policy(options) == OcrPolicy::Auto
                && options.error_policy == ErrorPolicy::BestEffort
                && auto_degradable_normalization(&error) =>
        {
            return Ok((
                CachedContribution {
                    diagnostics: vec![visual_diagnostic(None, error.to_string())],
                    ..Default::default()
                },
                None,
            ));
        }
        Err(error) => return Err(error),
    };
    let identity = OcrInputIdentity::try_new(
        Sha256::digest(&normalized.bytes).into(),
        normalized.width,
        normalized.height,
        0,
    )?;
    let mut diagnostics: Vec<_> =
        jpeg_input::diagnostic(normalized.trailing_bytes).into_iter().collect();
    let plan = services
        .ocr
        .as_deref()
        .ok_or_else(|| ConversionError::ComponentUnavailable {
            component: "ocr".into(),
            detail: "no OCR engine is configured".into(),
        })
        .and_then(|engine| {
            engine.planned_normalized_png_output(
                normalized.width,
                normalized.height,
                options,
                context,
            )
        });
    if let Err(error) = plan {
        if effective_ocr_policy(options) == OcrPolicy::Auto
            && options.error_policy == ErrorPolicy::BestEffort
            && matches!(error, ConversionError::ComponentUnavailable { .. })
        {
            diagnostics.push(visual_diagnostic(None, error.to_string()));
            return Ok((CachedContribution { diagnostics, ..Default::default() }, None));
        }
        return Err(error);
    }
    let contribution = crate::image_converter::ocr::recognize_for_input(
        &normalized.bytes,
        u32::try_from(ordinal + 1)
            .map_err(|_| resource("max_pages", "OCR page ordinal overflow"))?,
        normalized.width,
        normalized.height,
        identity,
        options,
        services,
        context,
    )
    .await?;
    diagnostics.extend(contribution.diagnostics);
    Ok((
        CachedContribution {
            nodes: contribution.nodes,
            diagnostics,
            telemetry: Some((
                identity,
                contribution.recognized_regions,
                contribution.recognized_chars,
            )),
        },
        contribution.memory,
    ))
}

fn native_inlines(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Text { value, .. } => has_body_characters(value),
        Inline::SourceText { value, provenance, .. } => {
            provenance.kind == ProvenanceKind::NativeParser && has_body_characters(value)
        }
        Inline::Link { content, .. } => native_inlines(content),
        _ => false,
    })
}

fn has_body_characters(value: &str) -> bool {
    value.chars().any(|character| !character.is_whitespace() && !character.is_control())
}

fn same_content_unit(left: &SourceLocator, right: &SourceLocator, format: InputFormat) -> bool {
    match format {
        // Each notebook output is its own content unit; source code in another
        // output or cell cannot replace text carried by an image.
        InputFormat::Ipynb => {
            left.part.is_some()
                && notebook_unit(left.part.as_deref()) == notebook_unit(right.part.as_deref())
        }
        // Flat archive merges and EPUB spines retain source-part identity.
        InputFormat::Zip | InputFormat::Epub => left.part.is_some() && left.part == right.part,
        _ => true,
    }
}

fn notebook_unit(part: Option<&str>) -> Option<&str> {
    let part = part?;
    if let Some((unit, _)) = part.split_once("/attachments/") {
        return Some(unit);
    }
    Some(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pdf_layout_body_belongs_to_the_same_page_as_pdfium_images() {
        let image = Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "builtin.converter.pdfium".into(),
            locator: SourceLocator { page: Some(1), ..Default::default() },
            confidence: None,
        };
        let mut body = BlockNode {
            id: NodeId("body".into()),
            block: Block::Paragraph(vec![Inline::Text {
                value: "native PDF body".into(),
                marks: vec![],
            }]),
            provenance: Provenance { provider: "builtin.pdf.layout".into(), ..image.clone() },
        };
        let context = ExecutionContext::new(Default::default(), Default::default());
        assert!(
            has_native_body(std::slice::from_ref(&body), &image, InputFormat::Pdf, &context)
                .unwrap()
        );
        body.provenance.locator.page = Some(2);
        assert!(
            !has_native_body(std::slice::from_ref(&body), &image, InputFormat::Pdf, &context)
                .unwrap()
        );
        body.provenance.locator.page = Some(1);
        body.provenance.kind = ProvenanceKind::LocalOcr;
        assert!(!has_native_body(&[body], &image, InputFormat::Pdf, &context).unwrap());
    }

    #[test]
    fn omitted_images_without_source_parts_have_distinct_logical_references() {
        let mut diagnostic = Diagnostic {
            code: "ocr.optionalRecognitionMemorySkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "private allowance".into(),
            locator: Some(SourceLocator { page: Some(2), ..Default::default() }),
        };
        let mut second = diagnostic.clone();
        locate_omission(&mut diagnostic, 1);
        locate_omission(&mut second, 2);
        assert_eq!(diagnostic.locator.as_ref().unwrap().part.as_deref(), Some("document/image/2"));
        assert_eq!(second.locator.as_ref().unwrap().part.as_deref(), Some("document/image/3"));
        assert_eq!(second.locator.as_ref().unwrap().page, Some(2));
        assert_eq!(second.locator.as_ref().unwrap().byte_start, None);
        locate_omission(&mut second, 3);
        assert_eq!(second.locator.as_ref().unwrap().part.as_deref(), Some("document/image/3"));

        let mut resource = Diagnostic {
            code: "ocr.optionalRecognitionResourceSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "recognition width".into(),
            locator: Some(SourceLocator { page: Some(2), ..Default::default() }),
        };
        locate_omission(&mut resource, 3);
        assert_eq!(resource.locator.as_ref().unwrap().part.as_deref(), Some("document/image/4"));
    }

    #[test]
    fn parser_control_markers_cannot_authorize_optional_ocr() {
        assert!(!native_inlines(&[Inline::Text { value: "\u{8}\0\t\n ".into(), marks: vec![] }]));
        assert!(has_body_characters("正文"));
        assert!(has_body_characters("123"));
        assert!(!has_body_characters("\u{8}"));
    }

    #[test]
    fn notebook_outputs_archive_entries_and_epub_spines_keep_separate_body_evidence() {
        let locator = |part: &str| SourceLocator { part: Some(part.into()), ..Default::default() };
        assert!(!same_content_unit(
            &locator("cells/1"),
            &locator("cells/2/outputs/0"),
            InputFormat::Ipynb
        ));
        assert!(!same_content_unit(
            &locator("cells/1"),
            &locator("cells/1/outputs/0"),
            InputFormat::Ipynb
        ));
        assert!(same_content_unit(
            &locator("cells/1"),
            &locator("cells/1/attachments/chart.png"),
            InputFormat::Ipynb
        ));
        for format in [InputFormat::Zip, InputFormat::Epub] {
            assert!(!same_content_unit(&locator("body.txt"), &locator("scan.png"), format));
            assert!(!same_content_unit(
                &SourceLocator::default(),
                &SourceLocator::default(),
                format
            ));
        }
    }

    #[test]
    fn another_ai_contribution_cannot_be_native_body_evidence() {
        let provenance = Provenance {
            kind: ProvenanceKind::AiProvider,
            provider: "remote".into(),
            locator: SourceLocator::default(),
            confidence: None,
        };
        assert!(!native_inlines(&[Inline::SourceText {
            value: "previous OCR".into(),
            marks: vec![],
            provenance: Box::new(provenance)
        }]));
        assert!(native_inlines(&[Inline::Link {
            target: "#body".into(),
            content: vec![Inline::Text { value: "native linked body".into(), marks: vec![] }]
        }]));
    }
}
