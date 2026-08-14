use crate::budget::LayoutBudget;
use crate::geometry::overlap_ratio;
use crate::lines;
use crate::memory;
use crate::model::{Line, SourceKind};
use into_markdown_core::ConversionError;
use unicode_normalization::UnicodeNormalization;

pub(crate) fn suppress(
    lines: Vec<Line>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    let mut kept: Vec<Line> = Vec::new();
    kept.try_reserve_exact(lines.len()).map_err(|_| memory("layout dedup allocation"))?;
    for line in lines {
        budget.checkpoint_item()?;
        let normalized = canonical(&lines::text(&line), budget)?;
        let mut duplicate = None;
        for (index, existing) in kept.iter().enumerate().rev().take(64) {
            budget.compare()?;
            if existing.orientation != line.orientation
                || overlap_ratio(existing.bounds, line.bounds) < 0.55
            {
                continue;
            }
            if normalized == canonical(&lines::text(existing), budget)? {
                duplicate = Some(index);
                break;
            }
        }
        if let Some(index) = duplicate {
            if kept[index].source_kind == SourceKind::Ocr && line.source_kind == SourceKind::Native
            {
                kept[index] = line;
            }
        } else {
            kept.push(line);
        }
    }
    Ok(kept)
}

fn canonical(value: &str, budget: &mut LayoutBudget<'_>) -> Result<String, ConversionError> {
    budget.checkpoint_bytes(value.len())?;
    let mut capacity = 0_usize;
    for character in value.nfc().filter(|character| !character.is_whitespace()) {
        for lowercase in character.to_lowercase() {
            capacity = capacity
                .checked_add(lowercase.len_utf8())
                .ok_or_else(|| memory("layout canonical length"))?;
        }
    }
    budget.checkpoint_bytes(value.len())?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| memory("layout canonical allocation"))?;
    for character in value.nfc() {
        if !character.is_whitespace() {
            output.extend(character.to_lowercase());
        }
    }
    Ok(output)
}
