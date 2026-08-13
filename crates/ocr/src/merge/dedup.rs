use super::budget::MergeBudget;
use super::geometry::{rect_overlap_ratio, union_rect};
use super::page_scope::PageScope;
use super::{Candidate, diagnostic};
use into_markdown_core::{
    Block, BlockNode, ConversionError, Diagnostic, Inline, Rect, SourceLocator,
};
use unicode_normalization::UnicodeNormalization as _;

pub(crate) struct NativeSpan {
    canonical: CanonicalText,
    bounds: Rect,
}

struct CanonicalText {
    value: String,
    scalar_count: usize,
}

pub(crate) fn collect_native_spans(
    blocks: &[BlockNode],
    page: u32,
    explicitly_scoped: bool,
    budget: &MergeBudget<'_>,
) -> Result<Vec<NativeSpan>, ConversionError> {
    fn inline_parts<'a>(
        values: &'a [Inline],
        bounds: &mut Option<Rect>,
        page: u32,
        scope: PageScope,
        budget: &MergeBudget<'_>,
        visited: &mut usize,
    ) -> Result<Vec<&'a str>, ConversionError> {
        let mut stack = Vec::new();
        stack.try_reserve_exact(values.len()).map_err(|_| super::memory())?;
        stack.extend(values.iter().rev());
        let mut parts = Vec::<&str>::new();
        while let Some(value) = stack.pop() {
            *visited = visited.checked_add(1).ok_or_else(super::memory)?;
            super::traversal_checkpoint(budget.context(), *visited)?;
            match value {
                Inline::SourceText { value, provenance, .. }
                | Inline::OcrText { value, provenance, .. }
                    if scope.includes_plain_text()
                        || (scope == PageScope::InlineFallback
                            && provenance.locator.page == Some(page)) =>
                {
                    parts.try_reserve(1).map_err(|_| super::memory())?;
                    parts.push(value.as_str());
                    if let Some(value) = provenance.locator.bounds {
                        *bounds = Some(bounds.map_or(value, |current| union_rect(current, value)));
                    }
                }
                Inline::Text { value, .. } | Inline::Code(value) | Inline::Formula(value)
                    if scope.includes_plain_text() =>
                {
                    parts.try_reserve(1).map_err(|_| super::memory())?;
                    parts.push(value.as_str());
                }
                Inline::Link { content, .. } => {
                    stack.try_reserve(content.len()).map_err(|_| super::memory())?;
                    stack.extend(content.iter().rev());
                }
                _ => {}
            }
        }
        Ok(parts)
    }
    let mut spans = Vec::new();
    let mut stack = Vec::new();
    stack.try_reserve_exact(1).map_err(|_| super::memory())?;
    stack.push((blocks, PageScope::root(explicitly_scoped)));
    let mut visited = 0_usize;
    while let Some((values, parent_scope)) = stack.pop() {
        for node in values {
            visited += 1;
            super::traversal_checkpoint(budget.context(), visited)?;
            let scope = parent_scope.for_node(&node.provenance, page);
            if scope == PageScope::Excluded {
                continue;
            }
            match &node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => {
                    let mut bounds = None;
                    let parts =
                        inline_parts(values, &mut bounds, page, scope, budget, &mut visited)?;
                    if !parts.is_empty()
                        && let Some(bounds) = bounds.or_else(|| {
                            scope
                                .includes_plain_text()
                                .then_some(node.provenance.locator.bounds)
                                .flatten()
                        })
                    {
                        spans.try_reserve(1).map_err(|_| super::memory())?;
                        spans.push(NativeSpan { canonical: canonical_parts(&parts)?, bounds });
                    }
                }
                Block::List { items, .. } => {
                    for item in items {
                        stack.try_reserve(1).map_err(|_| super::memory())?;
                        stack.push((item.blocks.as_slice(), scope));
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        stack.try_reserve(1).map_err(|_| super::memory())?;
                        stack.push((cell.blocks.as_slice(), scope));
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => {
                    stack.try_reserve(1).map_err(|_| super::memory())?;
                    stack.push((blocks.as_slice(), scope));
                }
                _ => {}
            }
        }
    }
    Ok(spans)
}

pub(crate) fn suppress_duplicates(
    mut candidates: Vec<Candidate>,
    native: &[NativeSpan],
    page: u32,
    page_width: f32,
    page_height: f32,
    budget: &mut MergeBudget<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<Candidate>, ConversionError> {
    let mut canonical = Vec::new();
    canonical.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    for candidate in &candidates {
        canonical.push(canonical_text(&candidate.text)?);
    }

    let mut suppressed = Vec::new();
    suppressed.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    suppressed.resize(candidates.len(), false);
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    ordered.extend(0..candidates.len());
    ordered.sort_by(|left, right| {
        candidates[*left]
            .geometry
            .bounds
            .x
            .total_cmp(&candidates[*right].geometry.bounds.x)
            .then_with(|| candidates[*left].source_index.cmp(&candidates[*right].source_index))
    });
    for (position, &left_index) in ordered.iter().enumerate() {
        let left_right =
            candidates[left_index].geometry.bounds.x + candidates[left_index].geometry.bounds.width;
        for &right_index in &ordered[position + 1..] {
            if candidates[right_index].geometry.bounds.x > left_right {
                break;
            }
            budget.consume(1)?;
            if !text_equivalent(&canonical[left_index], &canonical[right_index])
                || super::geometry::polygon_overlap_ratio(
                    &candidates[left_index].geometry.polygon,
                    &candidates[right_index].geometry.polygon,
                ) < 0.55
            {
                continue;
            }
            let keep_left = candidate_precedes(&candidates[left_index], &candidates[right_index]);
            let dropped = if keep_left { right_index } else { left_index };
            suppressed[dropped] = true;
        }
    }

    for (index, candidate) in candidates.iter().enumerate() {
        if suppressed[index] {
            diagnostics.push(diagnostic(
                "ocr.duplicateSuppressed",
                "overlapping OCR region was suppressed as a duplicate",
                locator(page, page_width, page_height, candidate.geometry.bounds),
            ));
            continue;
        }
        for span in native {
            if rect_overlap_ratio(candidate.geometry.bounds, span.bounds) < 0.45 {
                continue;
            }
            budget.consume(1)?;
            if text_equivalent(&canonical[index], &span.canonical) {
                suppressed[index] = true;
                diagnostics.push(diagnostic(
                    "ocr.nativeDuplicateSuppressed",
                    "OCR text overlapping equivalent native text was suppressed",
                    locator(page, page_width, page_height, candidate.geometry.bounds),
                ));
                break;
            }
        }
    }

    let mut retained = Vec::new();
    retained.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    for (index, candidate) in candidates.drain(..).enumerate() {
        if !suppressed[index] {
            retained.push(candidate);
        }
    }
    Ok(retained)
}

fn canonical_text(value: &str) -> Result<CanonicalText, ConversionError> {
    let capacity = value.nfc().try_fold(0_usize, |total, character| {
        if character.is_whitespace() {
            Ok(total)
        } else {
            total.checked_add(character.len_utf8()).ok_or_else(super::memory)
        }
    })?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| super::memory())?;
    let mut scalar_count = 0_usize;
    for character in value.nfc() {
        if !character.is_whitespace() {
            output.push(character);
            scalar_count += 1;
        }
    }
    Ok(CanonicalText { value: output, scalar_count })
}

fn canonical_parts(parts: &[&str]) -> Result<CanonicalText, ConversionError> {
    let capacity = parts.iter().flat_map(|part| part.chars()).nfc().try_fold(
        0_usize,
        |total, character| {
            if character.is_whitespace() {
                Ok(total)
            } else {
                total.checked_add(character.len_utf8()).ok_or_else(super::memory)
            }
        },
    )?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| super::memory())?;
    let mut scalar_count = 0_usize;
    for character in parts.iter().flat_map(|part| part.chars()).nfc() {
        if !character.is_whitespace() {
            output.push(character);
            scalar_count += 1;
        }
    }
    Ok(CanonicalText { value: output, scalar_count })
}

fn text_equivalent(left: &CanonicalText, right: &CanonicalText) -> bool {
    !left.value.is_empty()
        && !right.value.is_empty()
        && (left.value == right.value
            || (left.scalar_count >= 4 && right.value.contains(&left.value))
            || (right.scalar_count >= 4 && left.value.contains(&right.value)))
}

fn candidate_precedes(left: &Candidate, right: &Candidate) -> bool {
    let left_confidence = left.detection_confidence.min(left.recognition_confidence);
    let right_confidence = right.detection_confidence.min(right.recognition_confidence);
    left_confidence
        .total_cmp(&right_confidence)
        .reverse()
        .then_with(|| left.source_index.cmp(&right.source_index))
        .is_lt()
}

fn locator(page: u32, page_width: f32, page_height: f32, bounds: Rect) -> SourceLocator {
    SourceLocator {
        page: Some(page),
        bounds: Some(bounds),
        page_width: Some(page_width),
        page_height: Some(page_height),
        ..SourceLocator::default()
    }
}
