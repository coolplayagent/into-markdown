use crate::budget::LayoutBudget;
use crate::collect;
use crate::model::RebuiltBlock;
use crate::{
    LayoutConfig, dedup, footnotes, gutters, lines, malformed, memory, reading_order, semantics,
    tables,
};
use into_markdown_core::{BlockNode, ConversionError, Provenance, Rect};

pub(crate) fn reconstruct_page(
    page: u32,
    mut blocks: Vec<BlockNode>,
    page_provenance: &Provenance,
    path_bounds: &[Rect],
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
    // Page-render OCR and embedded-image OCR can describe the same pixels with
    // different line segmentation. Remove overlapping OCR atoms before line
    // clustering; once combined into one line, exact line-level deduplication
    // can no longer distinguish those duplicate observations.
    let atoms = dedup::suppress_overlapping_ocr_atoms(content.atoms, budget)?;
    let clustered = lines::cluster(atoms, budget)?;
    let deduplicated = dedup::suppress(clustered, budget)?;
    let median_font = semantics::font_baseline(&deduplicated, budget)?;
    // Lock locally corroborated two-dimensional grids before interpreting
    // repeated page-wide gaps as flowing columns. Weak grids remain text and
    // are eligible for column splitting.
    let (mut table_blocks, remaining) =
        tables::recover(deduplicated, path_bounds, page, width, height, config, budget)?;
    let split = gutters::split(remaining, width, budget)?;
    let ordered = reading_order::lines(split, width, height, budget)?;
    let mut rebuilt = semantics::blocks(page, ordered, width, height, median_font, budget)?;
    rebuilt
        .try_reserve_exact(table_blocks.len() + content.passthrough.len())
        .map_err(|_| memory("layout rebuilt page"))?;
    rebuilt.append(&mut table_blocks);
    crate::ids::avoid_retained_collisions(&mut rebuilt, &content.passthrough, budget)?;
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
