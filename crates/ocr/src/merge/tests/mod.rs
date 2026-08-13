mod contracts;
mod dedup;
mod geometry;
mod quality_authority;
mod resources;
mod wire;

use super::*;
use crate::{CropDescriptor, DetectedTextRegion, DetectionResult, PageDetection, RecognizedText};
use into_markdown_core::{
    Block, BlockNode, Document, ExecutionContext, ExecutionOptions, NodeId, Provenance,
    ProvenanceKind, Rect, ResourceLimits, SourceLocator,
};
use std::sync::Arc;

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn polygon(x: f32, y: f32, width: f32, height: f32) -> [(f32, f32); 4] {
    [(x, y), (x + width, y), (x + width, y + height), (x, y + height)]
}

fn detection(regions: &[([(f32, f32); 4], f32)]) -> DetectionResult {
    DetectionResult {
        regions: regions
            .iter()
            .map(|(polygon, confidence)| DetectedTextRegion {
                polygon: *polygon,
                angle_degrees: 0.0,
                confidence: *confidence,
                crop: CropDescriptor { polygon: *polygon, width: 100, height: 20 },
            })
            .collect(),
        provider: "test.detector".into(),
    }
}

fn recognition(regions: &[(usize, &str, f32)]) -> RecognitionResult {
    RecognitionResult {
        regions: Arc::from(
            regions
                .iter()
                .map(|(source_index, text, confidence)| RecognizedText {
                    source_index: *source_index,
                    text: (*text).into(),
                    confidence: *confidence,
                })
                .collect::<Vec<_>>(),
        ),
        provider: Arc::from("test.recognizer"),
        language_hint: None,
        _memory_lease: None,
        batch_identity: None,
        recognizer_model: None,
    }
}

fn input(detection: &DetectionResult, recognition: &RecognitionResult) -> OcrPageInput {
    input_for_page(1, detection, recognition)
}

fn input_for_page(
    page: u32,
    detection: &DetectionResult,
    recognition: &RecognitionResult,
) -> OcrPageInput {
    let detection = PageDetection::from_result(
        page,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    let mut recognition = recognition.clone();
    recognition.batch_identity = Some(detection.identity.clone());
    recognition.recognizer_model = Some(crate::batch::RECOGNIZER_MODEL_ID);
    OcrPageInput::new(detection, recognition).unwrap()
}

fn page_document(inlines: Vec<into_markdown_core::Inline>) -> Document {
    Document {
        blocks: vec![BlockNode {
            id: NodeId("page-1".into()),
            block: Block::Page {
                number: 1,
                blocks: if inlines.is_empty() {
                    Vec::new()
                } else {
                    vec![BlockNode {
                        id: NodeId("native".into()),
                        block: Block::Paragraph(inlines),
                        provenance: native_provenance(None),
                    }]
                },
            },
            provenance: native_provenance(None),
        }],
        ..Document::default()
    }
}

fn native_provenance(bounds: Option<Rect>) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "test.native".into(),
        locator: SourceLocator {
            page: Some(1),
            bounds,
            page_width: Some(600.0),
            page_height: Some(800.0),
            ..SourceLocator::default()
        },
        confidence: Some(1.0),
    }
}

fn merged_text(document: &Document) -> String {
    fn visit(blocks: &[BlockNode], output: &mut String) {
        for node in blocks {
            match &node.block {
                Block::Paragraph(inlines) => {
                    for inline in inlines {
                        match inline {
                            into_markdown_core::Inline::OcrText { value, .. } => {
                                output.push_str(value);
                            }
                            into_markdown_core::Inline::LineBreak => output.push('\n'),
                            _ => {}
                        }
                    }
                }
                Block::Page { blocks, .. } => visit(blocks, output),
                _ => {}
            }
        }
    }
    let mut output = String::new();
    visit(&document.blocks, &mut output);
    output
}
