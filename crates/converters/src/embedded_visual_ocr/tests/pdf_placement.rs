use super::*;
use into_markdown_core::{ErrorPolicy, Rect};

fn with_native_text(mut source: ConverterOutput) -> ConverterOutput {
    let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else { panic!("page") };
    blocks.insert(
        0,
        BlockNode {
            id: NodeId("native-text".into()),
            block: Block::Paragraph(
                "native words retained"
                    .chars()
                    .enumerate()
                    .map(|(index, value)| {
                        let mut source = provenance("native");
                        source.locator.bounds = Some(Rect {
                            x: 10.0 + 8.0 * f32::from(u16::try_from(index).unwrap()),
                            y: 120.0,
                            width: 8.0,
                            height: 12.0,
                        });
                        source.locator.font_size = Some(12.0);
                        Inline::SourceText {
                            value: value.to_string(),
                            marks: Vec::new(),
                            provenance: Box::new(source),
                        }
                    })
                    .collect(),
            ),
            provenance: provenance("native"),
        },
    );
    source
}

fn text(document: &Document) -> String {
    fn visit(nodes: &[BlockNode], value: &mut String) {
        for node in nodes {
            match &node.block {
                Block::Page { blocks, .. } => visit(blocks, value),
                Block::Paragraph(inlines) => {
                    for inline in inlines {
                        if let Inline::SourceText { value: text, .. }
                        | Inline::OcrText { value: text, .. } = inline
                        {
                            value.push_str(text);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut value = String::new();
    visit(&document.blocks, &mut value);
    value
}

#[test]
fn pdf_unmappable_placement_keeps_native_text_and_other_use_of_same_asset() {
    for placement in [
        Rect { x: -53.16, y: 20.0, width: 815.16, height: 60.0 },
        Rect { x: 590.0, y: 20.0, width: 30.0, height: 20.0 },
    ] {
        let mut source = with_native_text(output());
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page")
        };
        blocks[1].provenance.locator.bounds = Some(placement);
        // Reusing the same ID must not either drop the good placement or
        // accidentally attach its cached OCR to the rejected one.
        let Block::Image { asset, .. } = &mut blocks[2].block else { panic!("image") };
        *asset = AssetId("asset-a".into());
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let result =
            block_on(enrich(source, InputFormat::Pdf, &options, &services, &context)).unwrap();
        result.document.validate().unwrap();
        let value = text(&result.document);
        assert!(value.contains("native words retained"));
        assert_eq!(value.matches("embedded words").count(), 1);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            result.diagnostics.iter().filter(|d| d.code == "pdf.optionalOcrSkipped").count(),
            1
        );
        drop(result);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn pdf_only_unmappable_images_skip_recognition_and_preserve_original_document() {
    for rotation in [None, Some(90.0)] {
        let mut source = with_native_text(output());
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page")
        };
        for node in blocks.iter_mut().skip(1) {
            if rotation.is_some() {
                node.provenance.locator.rotation_degrees = rotation;
            } else {
                node.provenance.locator.page_width = None;
            }
        }
        let expected = source.document.clone();
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let result =
            block_on(enrich(source, InputFormat::Pdf, &options, &services, &context)).unwrap();
        assert_eq!(result.document, expected);
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn pdf_unmappable_required_or_strict_ocr_is_not_silently_omitted() {
    for (error_policy, policy) in
        [(ErrorPolicy::Strict, OcrPolicy::Auto), (ErrorPolicy::BestEffort, OcrPolicy::Always)]
    {
        let mut source = output();
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page")
        };
        blocks[0].provenance.locator.rotation_degrees = Some(90.0);
        let mut options = ConversionOptions { error_policy, ..ConversionOptions::default() };
        options.ocr.policy = policy;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        assert!(matches!(
            block_on(enrich(source, InputFormat::Pdf, &options, &services, &context)),
            Err(ConversionError::Unsupported { .. })
        ));
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn pdf_empty_ocr_contributions_keep_native_text_without_geometry_error() {
    for missing_provider in [false, true] {
        let source = with_native_text(output());
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.ocr.minimum_confidence = 0.999;
        let services = if missing_provider {
            Services::default()
        } else {
            Services { ocr: Some(source_bound_ocr(false)), ..Services::default() }
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let result =
            block_on(enrich(source, InputFormat::Pdf, &options, &services, &context)).unwrap();
        result.document.validate().unwrap();
        assert!(text(&result.document).contains("native words retained"));
        assert!(!text(&result.document).contains("embedded words"));
    }
}
