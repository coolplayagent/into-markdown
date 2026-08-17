use crate::budget::LayoutBudget;
use crate::geometry::overlap_ratio;
use crate::lines;
use crate::memory;
use crate::model::{Atom, Line, SourceKind};
use crate::ordering;
use into_markdown_core::ConversionError;
use unicode_normalization::UnicodeNormalization;

pub(crate) fn suppress_overlapping_ocr_atoms(
    atoms: Vec<Atom>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Atom>, ConversionError> {
    let mut kept = Vec::<AtomCandidate>::new();
    kept.try_reserve_exact(atoms.len()).map_err(|_| memory("layout atom dedup output"))?;
    for atom in atoms {
        budget.checkpoint_item()?;
        let normalized = canonical(lines::inline_text(&atom.inline), budget)?;
        let mut duplicate = false;
        if atom.source_kind == SourceKind::Ocr && !normalized.is_empty() {
            for existing in kept.iter().rev() {
                budget.compare()?;
                if existing.atom.source_kind != SourceKind::Ocr
                    || existing.atom.orientation != atom.orientation
                    || overlap_ratio(existing.atom.bounds, atom.bounds) < 0.55
                {
                    continue;
                }
                if equivalent_ocr_text(&existing.normalized, &normalized) {
                    duplicate = true;
                    break;
                }
            }
        }
        if !duplicate {
            kept.push(AtomCandidate { atom, normalized });
        }
    }
    let mut output = Vec::new();
    output.try_reserve_exact(kept.len()).map_err(|_| memory("layout atom dedup atoms"))?;
    output.extend(kept.into_iter().map(|candidate| candidate.atom));
    Ok(output)
}

struct AtomCandidate {
    atom: Atom,
    normalized: String,
}

fn equivalent_ocr_text(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let (shorter, longer) = if left.len() <= right.len() { (left, right) } else { (right, left) };
    shorter.chars().take(3).count() == 3 && longer.contains(shorter)
}

pub(crate) fn suppress(
    lines: Vec<Line>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    let mut candidates = Vec::new();
    candidates.try_reserve_exact(lines.len()).map_err(|_| memory("layout dedup candidates"))?;
    for (sequence, line) in lines.into_iter().enumerate() {
        budget.checkpoint_item()?;
        let text = lines::fallible_text(&line, budget)?;
        let normalized = canonical(&text, budget)?;
        candidates.push(Candidate { line, normalized, sequence });
    }
    ordering::by(&mut candidates, budget, |left, right| {
        left.line
            .orientation
            .cmp(&right.line.orientation)
            .then_with(|| left.line.bounds.y.total_cmp(&right.line.bounds.y))
            .then_with(|| left.line.bounds.x.total_cmp(&right.line.bounds.x))
            .then_with(|| {
                source_rank(left.line.source_kind).cmp(&source_rank(right.line.source_kind))
            })
            .then_with(|| left.line.source_index.cmp(&right.line.source_index))
            .then_with(|| left.sequence.cmp(&right.sequence))
    })?;

    let mut kept: Vec<Candidate> = Vec::new();
    kept.try_reserve_exact(candidates.len()).map_err(|_| memory("layout dedup output"))?;
    let mut active: Vec<usize> = Vec::new();
    active.try_reserve_exact(candidates.len()).map_err(|_| memory("layout dedup spatial index"))?;
    let mut orientation = None;
    for candidate in candidates {
        budget.checkpoint_item()?;
        if orientation != Some(candidate.line.orientation) {
            active.clear();
            orientation = Some(candidate.line.orientation);
        }
        let current_y = candidate.line.bounds.y;
        let mut retained = 0_usize;
        for read in 0..active.len() {
            budget.checkpoint_item()?;
            let index = active[read];
            let existing: &Candidate = &kept[index];
            if existing.line.bounds.y + existing.line.bounds.height + 0.5 >= current_y {
                active[retained] = index;
                retained += 1;
            }
        }
        active.truncate(retained);

        let mut duplicate = None;
        for &index in &active {
            budget.compare()?;
            let existing = &kept[index];
            if overlap_ratio(existing.line.bounds, candidate.line.bounds) < 0.55 {
                continue;
            }
            if existing.normalized == candidate.normalized {
                duplicate = Some(index);
                break;
            }
        }
        if let Some(index) = duplicate {
            if kept[index].line.source_kind == SourceKind::Ocr
                && candidate.line.source_kind == SourceKind::Native
            {
                kept[index] = candidate;
            }
        } else {
            let index = kept.len();
            kept.push(candidate);
            active.push(index);
        }
    }
    let mut output = Vec::new();
    output.try_reserve_exact(kept.len()).map_err(|_| memory("layout dedup lines"))?;
    output.extend(kept.into_iter().map(|candidate| candidate.line));
    Ok(output)
}

struct Candidate {
    line: Line,
    normalized: String,
    sequence: usize,
}

const fn source_rank(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Native => 0,
        SourceKind::Ocr => 1,
    }
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
