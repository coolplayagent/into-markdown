//! Recognition is transactional per byte identity; only controlled worker refusals are optional.

use super::*;
use into_markdown_core::{ErrorPolicy, Inline, ProvenanceKind};

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
            || origin.provider != image.provider
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

pub(super) fn optional_failure(
    error: &ConversionError,
    options: &ConversionOptions,
    references: &[VisualRef],
    asset_ids: impl Iterator<Item = AssetId>,
) -> bool {
    matches!(error, ConversionError::OcrRecognitionMemory { .. })
        && options.error_policy == ErrorPolicy::BestEffort
        && effective_ocr_policy(options) == OcrPolicy::Auto
        && asset_ids.into_iter().all(|asset| {
            references
                .iter()
                .filter(|reference| reference.asset == asset)
                .all(|reference| reference.optional)
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
