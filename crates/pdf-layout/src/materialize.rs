use crate::budget::LayoutBudget;
use crate::collect;
use crate::model::RebuiltBlock;
use crate::{
    LayoutConfig, dedup, footnotes, lines, malformed, memory, reading_order, semantics, tables,
};
use into_markdown_core::{BlockNode, ConversionError, Provenance};

pub(crate) fn reconstruct_page(
    page: u32,
    mut blocks: Vec<BlockNode>,
    page_provenance: &Provenance,
    config: &LayoutConfig,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<BlockNode>, ConversionError> {
    if page == 0 || page_provenance.locator.page != Some(page) {
        return Err(malformed("pdfLayoutPageIdentityMismatch"));
    }
    let width = page_provenance
        .locator
        .page_width
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| malformed("pdfLayoutMissingPageWidth"))?;
    let height = page_provenance
        .locator
        .page_height
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| malformed("pdfLayoutMissingPageHeight"))?;
    footnotes::namespace_existing(&mut blocks, page, budget)?;
    let content = collect::page_content(page, width, height, blocks, budget)?;
    if content.atoms.is_empty() {
        return Ok(content.passthrough);
    }
    let clustered = lines::cluster(content.atoms, budget)?;
    let deduplicated = dedup::suppress(clustered, budget)?;
    // A page-wide gutter is stronger layout evidence than repeated baselines.
    // Split columns before table inference so two flowing columns cannot be
    // promoted to a table merely because their rows happen to align.
    let split = lines::split_page_gutters(deduplicated, width, budget)?;
    let (mut table_blocks, remaining) =
        tables::recover(split, page, width, height, config, budget)?;
    let ordered = reading_order::lines(remaining, width, height, budget)?;
    let mut rebuilt = semantics::blocks(page, ordered, width, height, budget)?;
    rebuilt
        .try_reserve_exact(table_blocks.len() + content.passthrough.len())
        .map_err(|_| memory("layout rebuilt page"))?;
    rebuilt.append(&mut table_blocks);
    for (index, node) in content.passthrough.into_iter().enumerate() {
        rebuilt.push(RebuiltBlock {
            bounds: node.provenance.locator.bounds,
            orientation: 0,
            source_index: usize::MAX / 2 + index,
            node,
        });
    }
    let ordered = reading_order::blocks(rebuilt, width, height, budget)?;
    let mut output = Vec::new();
    output.try_reserve_exact(ordered.len()).map_err(|_| memory("layout output page"))?;
    output.extend(ordered.into_iter().map(|block| block.node));
    Ok(output)
}
