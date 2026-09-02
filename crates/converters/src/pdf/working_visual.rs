//! Internal ownership contract for full-page bitmaps created only for PDF OCR.

use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConverterOutput, Document, ExecutionContext,
    ProvenanceKind,
};
use std::collections::BTreeSet;

pub(crate) const ASSET_KIND: &str = "pdf-page-render";
pub(crate) const ASSET_PREFIX: &str = "pdf-page-render-";
const NODE_PREFIX: &str = "pdf-page-";
const NODE_SUFFIX: &str = "-ocr-render";
pub(crate) const ALT: &str = "page render for OCR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualRole {
    Published,
    OcrPageRender,
}

pub(crate) fn image_block(id: String) -> Block {
    Block::Image { asset: AssetId(id), alt: Some(ALT.into()) }
}

pub(crate) fn classify(node: &BlockNode) -> Result<VisualRole, ConversionError> {
    let Block::Image { asset, alt } = &node.block else { return Ok(VisualRole::Published) };
    let id_marked = node.id.0.starts_with(NODE_PREFIX) && node.id.0.ends_with(NODE_SUFFIX);
    let asset_marked = asset.0.starts_with(ASSET_PREFIX);
    let alt_marked = alt.as_deref() == Some(ALT);
    if !id_marked && !asset_marked && !alt_marked {
        return Ok(VisualRole::Published);
    }
    let page = node
        .id
        .0
        .strip_prefix(NODE_PREFIX)
        .and_then(|value| value.strip_suffix(NODE_SUFFIX))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|page| *page > 0)
        .ok_or_else(|| invalid_marker(node, "node ID"))?;
    let locator = &node.provenance.locator;
    let bounds = locator.bounds.ok_or_else(|| invalid_marker(node, "bounds"))?;
    let page_width = locator.page_width.ok_or_else(|| invalid_marker(node, "page width"))?;
    let page_height = locator.page_height.ok_or_else(|| invalid_marker(node, "page height"))?;
    if !asset_marked
        || !alt_marked
        || locator.page != Some(page)
        || node.provenance.kind != ProvenanceKind::NativeParser
        || node.provenance.provider != super::PROVIDER_ID
        || locator.rotation_degrees != Some(0.0)
        || bounds.x != 0.0
        || bounds.y != 0.0
        || bounds.width.to_bits() != page_width.to_bits()
        || bounds.height.to_bits() != page_height.to_bits()
    {
        return Err(invalid_marker(node, "coherent page-render provenance"));
    }
    Ok(VisualRole::OcrPageRender)
}

pub(crate) fn validate(
    document: &Document,
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<BTreeSet<AssetId>, ConversionError> {
    let mut working = BTreeSet::new();
    visit_nodes(&document.blocks, context, &mut |node| {
        if classify(node)? == VisualRole::OcrPageRender
            && let Block::Image { asset, .. } = &node.block
        {
            working.insert(asset.clone());
        }
        Ok(())
    })?;
    for asset in assets {
        context.checkpoint()?;
        if !asset.id.0.starts_with(ASSET_PREFIX) {
            continue;
        }
        if !working.contains(&asset.id)
            || asset.filename.as_deref().and_then(|filename| filename.strip_suffix(".bmp"))
                != Some(asset.id.0.as_str())
            || asset.media_type != "image/bmp"
            || asset.external_uri.is_some()
            || !asset.bytes.starts_with(b"BM")
        {
            return Err(ConversionError::Internal {
                detail: format!("invalid PDF OCR working asset {}", asset.id.0),
            });
        }
    }
    for id in &working {
        if !assets.iter().any(|asset| asset.id == *id) {
            return Err(ConversionError::Internal {
                detail: format!("PDF OCR working node references missing asset {}", id.0),
            });
        }
    }
    Ok(working)
}

pub(crate) fn discard(
    mut output: ConverterOutput,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let working = validate(&output.document, &output.assets, context)?;
    remove_nodes(&mut output.document.blocks, context)?;
    remove_assets(output, &working, context)
}

pub(crate) fn remove_assets(
    mut output: ConverterOutput,
    working: &BTreeSet<AssetId>,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if working.is_empty() {
        return Ok(output);
    }
    visit_nodes(&output.document.blocks, context, &mut |node| {
        if let Block::Image { asset, .. } = &node.block
            && working.contains(asset)
        {
            return Err(ConversionError::Internal {
                detail: format!(
                    "published node still references PDF OCR working asset {}",
                    asset.0
                ),
            });
        }
        Ok(())
    })?;
    output.assets.retain(|asset| !working.contains(&asset.id));
    output.reconcile_retained_output(context)
}

fn remove_nodes(
    nodes: &mut Vec<BlockNode>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut index = 0;
    while index < nodes.len() {
        context.checkpoint()?;
        match &mut nodes[index].block {
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. }
            | Block::Footnote { blocks, .. } => remove_nodes(blocks, context)?,
            Block::List { items, .. } => {
                for item in items {
                    remove_nodes(&mut item.blocks, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
                    remove_nodes(&mut cell.blocks, context)?;
                }
            }
            _ => {}
        }
        if classify(&nodes[index])? == VisualRole::OcrPageRender {
            nodes.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn visit_nodes(
    nodes: &[BlockNode],
    context: &ExecutionContext,
    visit: &mut impl FnMut(&BlockNode) -> Result<(), ConversionError>,
) -> Result<(), ConversionError> {
    for node in nodes {
        context.checkpoint()?;
        visit(node)?;
        match &node.block {
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. }
            | Block::Footnote { blocks, .. } => visit_nodes(blocks, context, visit)?,
            Block::List { items, .. } => {
                for item in items {
                    visit_nodes(&item.blocks, context, visit)?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter().flat_map(|row| &row.cells) {
                    visit_nodes(&cell.blocks, context, visit)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn invalid_marker(node: &BlockNode, detail: &str) -> ConversionError {
    ConversionError::Internal {
        detail: format!("PDF OCR working node {} lacks {detail}", node.id.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Document, ExecutionOptions, NodeId, Provenance, Rect, ResourceLimits, SourceLocator,
    };

    fn provenance(bounds: Rect) -> Provenance {
        Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: super::super::PROVIDER_ID.into(),
            locator: SourceLocator {
                page: Some(1),
                bounds: Some(bounds),
                rotation_degrees: Some(0.0),
                page_width: Some(100.0),
                page_height: Some(200.0),
                ..Default::default()
            },
            confidence: None,
        }
    }

    fn working_node(alt: Option<&str>) -> BlockNode {
        BlockNode {
            id: NodeId("pdf-page-1-ocr-render".into()),
            block: Block::Image {
                asset: AssetId(format!("{ASSET_PREFIX}fixture")),
                alt: alt.map(str::to_owned),
            },
            provenance: provenance(Rect { x: 0.0, y: 0.0, width: 100.0, height: 200.0 }),
        }
    }

    #[test]
    fn discard_removes_only_the_validated_working_visual_and_reconciles_memory() {
        let original = AssetId("pdf-image-original".into());
        let working = AssetId(format!("{ASSET_PREFIX}fixture"));
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("pdf-page-1".into()),
                block: Block::Page {
                    number: 1,
                    blocks: vec![
                        BlockNode {
                            id: NodeId("pdf-page-1-image-1".into()),
                            block: Block::Image { asset: original.clone(), alt: None },
                            provenance: provenance(Rect {
                                x: 10.0,
                                y: 20.0,
                                width: 30.0,
                                height: 40.0,
                            }),
                        },
                        working_node(Some(ALT)),
                    ],
                },
                provenance: provenance(Rect { x: 0.0, y: 0.0, width: 100.0, height: 200.0 }),
            }],
            ..Default::default()
        };
        let output = ConverterOutput::new(
            document,
            vec![
                Asset {
                    id: original.clone(),
                    filename: Some("original.bmp".into()),
                    media_type: "image/bmp".into(),
                    bytes: b"BM-original".to_vec(),
                    external_uri: None,
                },
                Asset {
                    id: working.clone(),
                    filename: Some(format!("{}.bmp", working.0)),
                    media_type: "image/bmp".into(),
                    bytes: b"BM-working".to_vec(),
                    external_uri: None,
                },
            ],
            vec![],
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let output = output.account_retained(&context).unwrap();
        let output = discard(output, &context).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].id, original);
        assert!(!serde_json::to_string(&output.document).unwrap().contains(ASSET_PREFIX));
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn partial_working_markers_fail_closed() {
        let node = working_node(None);
        assert!(matches!(classify(&node), Err(ConversionError::Internal { .. })));
    }
}
