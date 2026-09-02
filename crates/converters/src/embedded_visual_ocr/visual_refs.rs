//! Source-bound visual selection and published-asset accounting.

use super::{pdf_placement, resource, runtime, supported_raster};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    ExecutionContext, InputFormat, Provenance, ResourceReservation,
};
use std::collections::BTreeSet;

#[derive(Clone)]
pub(super) struct VisualRef {
    pub(super) optional: bool,
    pub(super) asset: AssetId,
    pub(super) provenance: Provenance,
    pub(super) role: crate::pdf::working_visual::VisualRole,
}

pub(super) struct PlanSelection {
    pub(super) references: Vec<VisualRef>,
    pub(super) working_assets: BTreeSet<AssetId>,
    pub(super) memory: ResourceReservation,
    pub(super) planned_bytes: u64,
    pub(super) selected_id_bytes: u64,
}

pub(super) fn plan(
    output: &ConverterOutput,
    expected_count: usize,
    expected_id_bytes: u64,
    input_format: InputFormat,
    context: &ExecutionContext,
) -> Result<PlanSelection, ConversionError> {
    let planned_bytes = u64::try_from(expected_count)
        .unwrap_or(u64::MAX)
        .checked_mul(u64::try_from(std::mem::size_of::<VisualRef>()).unwrap_or(u64::MAX) + 512)
        .and_then(|bytes| bytes.checked_add(expected_id_bytes))
        .ok_or_else(|| resource("max_memory_bytes", "OCR reference plan overflow"))?;
    let memory = context.reserve_memory(planned_bytes)?;
    let mut references = Vec::new();
    references.try_reserve_exact(expected_count).map_err(|error| {
        resource("max_memory_bytes", format!("cannot reserve OCR references: {error}"))
    })?;
    collect(&output.document.blocks, &mut references, input_format, context)?;
    let working_assets = select_pdf_sources(output, &mut references, input_format, context)?;
    let selected_id_bytes = references.iter().try_fold(0_u64, |total, reference| {
        total
            .checked_add(u64::try_from(reference.asset.0.len()).map_err(|_| {
                resource(
                    "max_memory_bytes",
                    "selected OCR reference ID length is not representable",
                )
            })?)
            .ok_or_else(|| resource("max_memory_bytes", "selected OCR reference IDs overflow"))
    })?;
    Ok(PlanSelection { references, working_assets, memory, planned_bytes, selected_id_bytes })
}

pub(super) fn select(
    output: &ConverterOutput,
    input_format: InputFormat,
    context: &ExecutionContext,
) -> Result<(Vec<VisualRef>, BTreeSet<AssetId>), ConversionError> {
    let mut references = Vec::new();
    collect(&output.document.blocks, &mut references, input_format, context)?;
    let working_assets = select_pdf_sources(output, &mut references, input_format, context)?;
    Ok((references, working_assets))
}

pub(super) fn discard_working(
    output: ConverterOutput,
    input_format: InputFormat,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if input_format == InputFormat::Pdf {
        crate::pdf::working_visual::discard(output, context)
    } else {
        Ok(output)
    }
}

fn select_pdf_sources(
    output: &ConverterOutput,
    references: &mut Vec<VisualRef>,
    input_format: InputFormat,
    context: &ExecutionContext,
) -> Result<BTreeSet<AssetId>, ConversionError> {
    if input_format != InputFormat::Pdf {
        return Ok(BTreeSet::new());
    }
    let working_assets =
        crate::pdf::working_visual::validate(&output.document, &output.assets, context)?;
    pdf_placement::select_page_sources(references, context)?;
    Ok(working_assets)
}

pub(super) fn account_published_bytes(
    total: &mut u64,
    asset: &Asset,
    working_assets: &BTreeSet<AssetId>,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if working_assets.contains(&asset.id) {
        return Ok(());
    }
    *total = total
        .checked_add(u64::try_from(asset.bytes.len()).map_err(|_| {
            resource("max_total_asset_bytes", "embedded visual byte count is not representable")
        })?)
        .ok_or_else(|| resource("max_total_asset_bytes", "embedded visual bytes overflow"))?;
    if *total > options.limits.max_total_asset_bytes {
        return Err(resource(
            "max_total_asset_bytes",
            "embedded visual bytes exceed the request limit",
        ));
    }
    Ok(())
}

fn collect(
    nodes: &[BlockNode],
    output: &mut Vec<VisualRef>,
    input_format: InputFormat,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for (index, node) in nodes.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        match &node.block {
            Block::Image { asset, .. } => {
                let role = if input_format == InputFormat::Pdf {
                    crate::pdf::working_visual::classify(node)?
                } else {
                    crate::pdf::working_visual::VisualRole::Published
                };
                output.push(VisualRef {
                    optional: runtime::has_native_body(
                        nodes,
                        &node.provenance,
                        input_format,
                        context,
                    )?,
                    asset: asset.clone(),
                    provenance: node.provenance.clone(),
                    role,
                });
            }
            Block::List { items, .. } => {
                for item in items {
                    collect(&item.blocks, output, input_format, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        collect(&cell.blocks, output, input_format, context)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect(blocks, output, input_format, context)?,
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn enforce_supported_reference_limit(
    assets: &[Asset],
    reference_counts: &super::ReferenceIndex,
    limit: u32,
) -> Result<(), ConversionError> {
    let count = assets.iter().try_fold(0_u64, |total, asset| {
        if asset.bytes.is_empty() || asset.external_uri.is_some() || !supported_raster(asset) {
            return Ok(total);
        }
        total.checked_add(reference_counts.count(&asset.id).unwrap_or(0)).ok_or_else(|| {
            resource("max_archive_entries", "embedded visual reference count is not representable")
        })
    })?;
    if count > u64::from(limit) {
        return Err(resource(
            "max_archive_entries",
            "embedded visual references exceed the request limit",
        ));
    }
    Ok(())
}
