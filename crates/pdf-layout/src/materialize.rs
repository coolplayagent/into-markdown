use crate::budget::LayoutBudget;
use crate::collect;
use crate::links::Links;
use crate::model::RebuiltBlock;
use crate::{
    LayoutConfig, dedup, footnotes, gutters, lines, malformed, memory, reading_order, semantics,
    tables, toc,
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
    let mut links = Links::extract(page, width, height, &mut blocks, budget)?;
    let content = collect::page_content(page, width, height, blocks, budget)?;
    if content.atoms.is_empty() {
        let mut retained = content.passthrough;
        links.associate(&[], budget)?;
        links.restore_unmatched(&mut retained, budget)?;
        return Ok(retained);
    }
    // Page-render OCR and embedded-image OCR can describe the same pixels with
    // different line segmentation. Remove overlapping OCR atoms before line
    // clustering; once combined into one line, exact line-level deduplication
    // can no longer distinguish those duplicate observations.
    let atoms = dedup::suppress_overlapping_ocr_atoms(content.atoms, budget)?;
    let clustered = lines::cluster(atoms, budget)?;
    let deduplicated = dedup::suppress(clustered, budget)?;
    let median_font = semantics::font_baseline(&deduplicated, budget)?;
    links.associate(&deduplicated, budget)?;
    let (mut directories, remaining) =
        toc::recover(deduplicated, &links, page, (width, height), budget)?;
    // Lock locally corroborated two-dimensional grids before interpreting
    // repeated page-wide gaps as flowing columns. Weak grids remain text and
    // are eligible for column splitting.
    let (mut table_blocks, remaining) =
        tables::recover(remaining, path_bounds, page, width, height, config, budget)?;
    check_unresolved_numeric_grid(&remaining, path_bounds, table_blocks.is_empty(), budget)?;
    let split = gutters::split(remaining, width, budget)?;
    let line_height = reading_order::line_height(&split, budget)?;
    let ordered = reading_order::lines(split, width, height, budget)?;
    let mut rebuilt = semantics::blocks(page, ordered, width, height, median_font, budget)?;
    rebuilt
        .try_reserve_exact(table_blocks.len() + directories.len())
        .map_err(|_| memory("layout rebuilt page"))?;
    rebuilt.append(&mut table_blocks);
    rebuilt.append(&mut directories);
    finish_blocks(rebuilt, content.passthrough, links, (width, height), line_height, budget)
}

fn finish_blocks(
    mut rebuilt: Vec<RebuiltBlock>,
    mut retained: Vec<BlockNode>,
    mut links: Links,
    dimensions: (f32, f32),
    line_height: Option<f32>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<BlockNode>, ConversionError> {
    for block in &mut rebuilt {
        links.attach(std::slice::from_mut(&mut block.node), budget)?;
    }
    links.restore_unmatched(&mut retained, budget)?;
    footnotes::resolve_collisions(&mut rebuilt, &retained, budget)?;
    crate::ids::avoid_retained_collisions(&mut rebuilt, &retained, budget)?;
    rebuilt.try_reserve_exact(retained.len()).map_err(|_| memory("layout retained blocks"))?;
    for (index, node) in retained.into_iter().enumerate() {
        rebuilt.push(RebuiltBlock {
            bounds: node.provenance.locator.bounds,
            orientation: 0,
            source_index: usize::MAX / 2 + index,
            node,
        });
    }
    let ordered = reading_order::blocks(rebuilt, dimensions.0, dimensions.1, line_height, budget)?;
    let mut output = Vec::new();
    output.try_reserve_exact(ordered.len()).map_err(|_| memory("layout output page"))?;
    output.extend(ordered.into_iter().map(|block| block.node));
    Ok(output)
}

/// Dense numeric rows left outside a corroborated table cannot safely become
/// flowing columns: column splitting can associate a score with the next label.
fn check_unresolved_numeric_grid(
    lines: &[crate::model::Line],
    path_bounds: &[Rect],
    no_recovered_table: bool,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut rows = 0;
    let mut text_rows = 0;
    let mut table_caption = false;
    let mut table_rules = 0;
    for bounds in path_bounds {
        budget.checkpoint_item()?;
        if (bounds.width > 100.0 && bounds.height <= 2.0)
            || (bounds.height > 100.0 && bounds.width <= 2.0)
        {
            table_rules += 1;
        }
    }
    for line in lines {
        budget.checkpoint_item()?;
        let mut digits = 0;
        let mut letters = 0;
        let mut gaps = 0;
        for atom in &line.atoms {
            budget.checkpoint_item()?;
            for value in crate::lines::inline_text(&atom.inline).chars() {
                digits += usize::from(value.is_ascii_digit());
                letters += usize::from(value.is_alphabetic());
            }
        }
        for pair in line.atoms.windows(2) {
            budget.compare()?;
            let gap = crate::geometry::major_start(pair[1].bounds, line.orientation)
                - crate::geometry::major_end(pair[0].bounds, line.orientation);
            if gap > line.font_size.unwrap_or(12.0) * 2.0 {
                gaps += 1;
            }
        }
        let text = crate::lines::fallible_text(line, budget)?;
        table_caption |= text.trim_start().starts_with("Table ")
            || text.split("Table ").skip(1).any(|suffix| {
                let number_end = suffix.find(|c: char| !c.is_ascii_digit()).unwrap_or(suffix.len());
                number_end > 0 && suffix[number_end..].starts_with([':', '.'])
            });
        if table_rules >= 2 {
            if letters >= 4 && gaps >= 1 {
                text_rows += 1;
            }
        }
        if (digits >= 6 && letters <= 70 && gaps >= 1) || numeric_tail(line, budget)? {
            rows += 1;
            if rows >= 24 {
                return Err(ConversionError::Malformed {
                    part: Some("pdf-layout-visual".into()),
                    detail: "unresolved dense numeric rows require a page image to preserve label-value alignment".into(),
                });
            }
        }
    }
    if table_caption && rows >= 3 {
        return Err(ConversionError::Malformed {
            part: Some("pdf-layout-visual".into()),
            detail: "partially recovered numeric table requires a page image to preserve remaining label-value associations".into(),
        });
    }
    if no_recovered_table && table_caption && text_rows >= 3 {
        return Err(ConversionError::Malformed {
            part: Some("pdf-layout-visual".into()),
            detail: "unresolved ruled text table requires a visual to preserve cell associations"
                .into(),
        });
    }
    Ok(())
}

// A table can occupy only one side of a line shared with prose. Inspect its
// trailing cell independently so the prose does not hide an unresolved score.
fn numeric_tail(
    line: &crate::model::Line,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    let mut digits = 0;
    let mut letters = 0;
    let mut uncertainty = false;
    for pair in line.atoms.windows(2).rev() {
        budget.compare()?;
        for value in crate::lines::inline_text(&pair[1].inline).chars() {
            digits += usize::from(value.is_ascii_digit());
            letters += usize::from(value.is_alphabetic());
            uncertainty |= value == '±';
        }
        if letters > 0 {
            return Ok(digits >= 6 && uncertainty);
        }
        let gap = crate::geometry::major_start(pair[1].bounds, line.orientation)
            - crate::geometry::major_end(pair[0].bounds, line.orientation);
        if gap > line.font_size.unwrap_or(12.0) * 2.0 {
            return Ok(digits >= 6 && letters == 0);
        }
    }
    Ok(false)
}
