use crate::{AssetSnapshot, SemanticReference, SourceStep};
use into_markdown_core::{Asset, Block, ConversionError, ExecutionContext, Inline, Provenance};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn references(
    block: &Block,
    context: &ExecutionContext,
) -> Result<Vec<SemanticReference>, ConversionError> {
    let mut references = Vec::new();
    match block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            inline_references(inlines, &mut references, context)?;
        }
        Block::Image { asset, .. } => {
            references.push(SemanticReference { kind: "asset".into(), target: asset.0.clone() });
        }
        _ => {}
    }
    references.sort();
    references.dedup();
    Ok(references)
}

fn inline_references(
    inlines: &[Inline],
    output: &mut Vec<SemanticReference>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for inline in inlines {
        context.checkpoint()?;
        match inline {
            Inline::Link { target, content } => {
                output.push(SemanticReference { kind: "link".into(), target: target.clone() });
                inline_references(content, output, context)?;
            }
            Inline::FootnoteReference(target) => {
                output.push(SemanticReference { kind: "footnote".into(), target: target.clone() });
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn source_chain(
    block: &Block,
    provenance: &Provenance,
    context: &ExecutionContext,
) -> Result<Vec<SourceStep>, ConversionError> {
    let mut output =
        vec![SourceStep { kind: provenance.kind, provider: provenance.provider.clone() }];
    match block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            inline_source_chain(inlines, &mut output, context)?;
        }
        _ => {}
    }
    output.dedup();
    Ok(output)
}

fn inline_source_chain(
    inlines: &[Inline],
    output: &mut Vec<SourceStep>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for inline in inlines {
        context.checkpoint()?;
        match inline {
            Inline::SourceText { provenance, .. } => output
                .push(SourceStep { kind: provenance.kind, provider: provenance.provider.clone() }),
            Inline::OcrText { provenance, evidence, .. } => {
                output.push(SourceStep {
                    kind: provenance.kind,
                    provider: provenance.provider.clone(),
                });
                for step in &evidence.chain {
                    context.checkpoint()?;
                    output.push(SourceStep {
                        kind: provenance.kind,
                        provider: step.model.as_ref().map_or_else(
                            || step.provider.clone(),
                            |model| format!("{}:{model}", step.provider),
                        ),
                    });
                }
            }
            Inline::Link { content, .. } => inline_source_chain(content, output, context)?,
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn assets(
    assets: &[Asset],
    referenced_assets: &BTreeSet<String>,
    context: &ExecutionContext,
) -> Result<Vec<AssetSnapshot>, ConversionError> {
    let mut snapshots = Vec::with_capacity(assets.len());
    for asset in assets {
        context.checkpoint()?;
        snapshots.push(AssetSnapshot {
            id: asset.id.0.clone(),
            media_type: asset.media_type.clone(),
            filename: asset.filename.clone(),
            external_uri: asset.external_uri.clone(),
            bytes: u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX),
            sha256: asset_sha256(&asset.bytes, context)?,
            referenced: referenced_assets.contains(&asset.id.0),
        });
    }
    snapshots.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(snapshots)
}

pub(crate) fn associate_named_attachments(
    nodes: &mut [crate::SemanticNode],
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<BTreeSet<String>, ConversionError> {
    let mut by_filename: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for asset in assets {
        context.checkpoint()?;
        if let Some(filename) = asset.filename.as_deref() {
            by_filename.entry(filename).or_default().push(asset.id.0.as_str());
        }
    }
    let mut referenced = BTreeSet::new();
    for node in nodes {
        context.checkpoint()?;
        if node.kind != "paragraph" {
            continue;
        }
        let Some(ids) = by_filename.get(node.text.as_str()) else {
            continue;
        };
        if let [id] = ids.as_slice() {
            node.references
                .push(SemanticReference { kind: "attachment".into(), target: (*id).into() });
            node.references.sort();
            node.references.dedup();
            referenced.insert((*id).into());
        }
    }
    Ok(referenced)
}

fn asset_sha256(bytes: &[u8], context: &ExecutionContext) -> Result<String, ConversionError> {
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        context.checkpoint()?;
        digest.update(chunk);
    }
    Ok(format!("{:x}", digest.finalize()))
}
