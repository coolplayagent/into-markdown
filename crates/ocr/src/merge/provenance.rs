use super::OcrPageInput;
use super::paragraphs::MergedParagraph;
use into_markdown_core::{
    Block, BlockNode, ConversionError, Inline, NodeId, OcrEvidence, OcrEvidenceStage,
    OcrEvidenceStep, OcrSourceRegion, Provenance, ProvenanceKind, SourceLocator,
};
use std::collections::BTreeSet;
use std::fmt::Write as _;

pub(crate) const MERGE_PROVIDER: &str = "builtin.ocr.ir-merge";

pub(crate) fn materialize_paragraphs(
    paragraphs: Vec<MergedParagraph>,
    page: &OcrPageInput,
    identifiers: &mut BTreeSet<String>,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut nodes = Vec::new();
    nodes.try_reserve_exact(paragraphs.len()).map_err(|_| super::memory())?;
    for (paragraph_index, paragraph) in paragraphs.into_iter().enumerate() {
        let mut content = Vec::new();
        let inline_count = paragraph.lines.len().checked_mul(2).ok_or_else(super::memory)?;
        content.try_reserve_exact(inline_count).map_err(|_| super::memory())?;
        let mut paragraph_confidence = 1.0_f32;
        for (line_index, line) in paragraph.lines.into_iter().enumerate() {
            if line_index > 0 {
                content.push(Inline::LineBreak);
            }
            let mut regions = Vec::new();
            regions.try_reserve_exact(line.candidates.len()).map_err(|_| super::memory())?;
            let mut line_confidence = 1.0_f32;
            for candidate in line.candidates {
                line_confidence = line_confidence
                    .min(candidate.detection_confidence)
                    .min(candidate.recognition_confidence);
                regions.push(OcrSourceRegion {
                    source_index: u32::try_from(candidate.source_index)
                        .map_err(|_| super::ocr("sourceIndexOverflow"))?,
                    polygon: candidate.geometry.polygon,
                    detection_confidence: candidate.detection_confidence,
                    recognition_confidence: candidate.recognition_confidence,
                });
            }
            paragraph_confidence = paragraph_confidence.min(line_confidence);
            let provenance = Provenance {
                kind: ProvenanceKind::LocalOcr,
                provider: clone_string(&page.recognition.provider)?,
                locator: SourceLocator {
                    page: Some(page.page()),
                    bounds: Some(line.bounds),
                    rotation_degrees: Some(line.angle_degrees.rem_euclid(360.0)),
                    page_width: Some(page.page_width()),
                    page_height: Some(page.page_height()),
                    ..SourceLocator::default()
                },
                confidence: Some(line_confidence),
            };
            content.push(Inline::OcrText {
                value: line.text,
                marks: Vec::new(),
                provenance: Box::new(provenance),
                evidence: Box::new(OcrEvidence {
                    page: page.page(),
                    regions,
                    chain: evidence_chain(page)?,
                }),
            });
        }
        nodes.push(BlockNode {
            id: unique_node_id(page.page(), paragraph_index, identifiers)?,
            block: Block::Paragraph(content),
            provenance: Provenance {
                kind: ProvenanceKind::Postprocessor,
                provider: clone_string(MERGE_PROVIDER)?,
                locator: SourceLocator {
                    page: Some(page.page()),
                    bounds: Some(paragraph.bounds),
                    page_width: Some(page.page_width()),
                    page_height: Some(page.page_height()),
                    ..SourceLocator::default()
                },
                confidence: Some(paragraph_confidence),
            },
        });
    }
    Ok(nodes)
}

pub(crate) fn page_provenance(page: &OcrPageInput) -> Result<Provenance, ConversionError> {
    Ok(Provenance {
        kind: ProvenanceKind::Postprocessor,
        provider: clone_string(MERGE_PROVIDER)?,
        locator: SourceLocator {
            page: Some(page.page()),
            page_width: Some(page.page_width()),
            page_height: Some(page.page_height()),
            ..SourceLocator::default()
        },
        confidence: None,
    })
}

fn evidence_chain(page: &OcrPageInput) -> Result<Vec<OcrEvidenceStep>, ConversionError> {
    let mut chain = Vec::new();
    chain.try_reserve_exact(3).map_err(|_| super::memory())?;
    chain.push(OcrEvidenceStep {
        stage: OcrEvidenceStage::Detection,
        provider: clone_string(&page.detected().provider)?,
        model: Some(clone_string(page.detection.identity.detector_model)?),
    });
    chain.push(OcrEvidenceStep {
        stage: OcrEvidenceStage::Recognition,
        provider: clone_string(&page.recognition.provider)?,
        model: Some(clone_string(page.recognition.recognizer_model.unwrap_or(""))?),
    });
    chain.push(OcrEvidenceStep {
        stage: OcrEvidenceStage::Merge,
        provider: clone_string(MERGE_PROVIDER)?,
        model: None,
    });
    Ok(chain)
}

fn unique_node_id(
    page: u32,
    paragraph: usize,
    identifiers: &mut BTreeSet<String>,
) -> Result<NodeId, ConversionError> {
    let mut base = String::new();
    base.try_reserve_exact(64).map_err(|_| super::memory())?;
    write!(&mut base, "ocr-page-{page}-paragraph-{}", paragraph + 1)
        .map_err(|_| super::memory())?;
    unique_identifier(&base, identifiers)
}

pub(crate) fn unique_page_id(
    page: u32,
    identifiers: &mut BTreeSet<String>,
) -> Result<NodeId, ConversionError> {
    let mut base = String::new();
    base.try_reserve_exact(32).map_err(|_| super::memory())?;
    write!(&mut base, "ocr-page-{page}").map_err(|_| super::memory())?;
    unique_identifier(&base, identifiers)
}

fn unique_identifier(
    base: &str,
    identifiers: &mut BTreeSet<String>,
) -> Result<NodeId, ConversionError> {
    for suffix in 0..=identifiers.len() {
        let value = if suffix == 0 {
            clone_string(base)?
        } else {
            let mut value = String::new();
            value.try_reserve_exact(base.len().saturating_add(24)).map_err(|_| super::memory())?;
            value.push_str(base);
            write!(&mut value, "-{suffix}").map_err(|_| super::memory())?;
            value
        };
        if !identifiers.contains(&value) {
            identifiers.insert(clone_string(&value)?);
            return Ok(NodeId(value));
        }
    }
    Err(super::ocr("nodeIdExhausted"))
}

fn clone_string(value: &str) -> Result<String, ConversionError> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| super::memory())?;
    output.push_str(value);
    Ok(output)
}
