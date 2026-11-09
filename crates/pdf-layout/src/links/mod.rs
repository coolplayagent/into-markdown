//! Page-local link evidence survives geometric and semantic reconstruction.
mod attach;
mod extract;

use crate::budget::LayoutBudget;
use crate::geometry::intersection_area;
use crate::model::{Line, SourceKind};
use crate::ordering;
use into_markdown_core::{BlockNode, ConversionError, Inline, Rect};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceKey {
    ocr: bool,
    index: Option<u32>,
    bounds: [u32; 4],
}

impl SourceKey {
    fn of(inline: &Inline) -> Option<Self> {
        let (provenance, ocr) = match inline {
            Inline::SourceText { provenance, .. } => (provenance, false),
            Inline::OcrText { provenance, .. } => (provenance, true),
            _ => return None,
        };
        let r = provenance.locator.bounds?;
        Some(Self {
            ocr,
            index: provenance.locator.character_index,
            bounds: [r.x.to_bits(), r.y.to_bits(), r.width.to_bits(), r.height.to_bits()],
        })
    }
}

struct Candidate {
    target: String,
    bounds: Option<Rect>,
    keys: Vec<SourceKey>,
    fallback: BlockNode,
    spatial: bool,
    state: Association,
    used: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Association {
    Ready,
    Duplicate,
    Conflict,
}

#[derive(Default)]
pub(crate) struct Links {
    candidates: Vec<Candidate>,
    assignments: Vec<(SourceKey, usize)>,
}

pub(crate) fn source_link(content: &[Inline]) -> bool {
    content.iter().any(|inline| SourceKey::of(inline).is_some())
        && content.iter().all(|inline| SourceKey::of(inline).is_some() || whitespace(inline))
}

fn whitespace(inline: &Inline) -> bool {
    match inline {
        Inline::Text { value, .. } | Inline::SourceText { value, .. } => {
            !value.is_empty() && value.chars().all(char::is_whitespace)
        }
        Inline::LineBreak => true,
        _ => false,
    }
}

impl Links {
    pub(crate) fn associate(
        &mut self,
        lines: &[Line],
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        for candidate in &mut self.candidates {
            if candidate.spatial {
                match_characters(candidate, lines, budget)?;
            }
            ordering::by(&mut candidate.keys, budget, Ord::cmp)?;
            candidate.keys.dedup();
        }
        self.resolve_overlaps(budget)?;
        for (index, candidate) in self.candidates.iter().enumerate() {
            if candidate.state != Association::Ready {
                continue;
            }
            for key in &candidate.keys {
                budget.push(&mut self.assignments, (*key, index))?;
            }
        }
        ordering::by(&mut self.assignments, budget, Ord::cmp)?;
        self.assignments.dedup_by_key(|entry| entry.0);
        Ok(())
    }

    fn resolve_overlaps(&mut self, budget: &mut LayoutBudget<'_>) -> Result<(), ConversionError> {
        for right in 0..self.candidates.len() {
            let (before, after) = self.candidates.split_at_mut(right);
            let current = &mut after[0];
            for prior in before {
                budget.compare()?;
                if prior.state == Association::Duplicate {
                    continue;
                }
                if prior.target == current.target {
                    let covered = !current.keys.is_empty()
                        && covered_keys(&current.keys, &prior.keys, budget)?;
                    let same_empty_region = current.keys.is_empty()
                        && prior.keys.is_empty()
                        && current.bounds.is_some()
                        && current.bounds == prior.bounds;
                    if covered || same_empty_region {
                        current.state = Association::Duplicate;
                        break;
                    }
                    if !prior.keys.is_empty() && covered_keys(&prior.keys, &current.keys, budget)? {
                        prior.state = Association::Duplicate;
                    }
                } else if intersect(&current.keys, &prior.keys, budget)? {
                    current.state = Association::Conflict;
                    prior.state = Association::Conflict;
                }
            }
        }
        Ok(())
    }

    fn assignment(
        &self,
        inline: &Inline,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<Option<usize>, ConversionError> {
        let Some(key) = SourceKey::of(inline) else { return Ok(None) };
        let (mut left, mut right) = (0, self.assignments.len());
        while left < right {
            budget.compare()?;
            let middle = left + (right - left) / 2;
            match self.assignments[middle].0.cmp(&key) {
                std::cmp::Ordering::Less => left = middle + 1,
                std::cmp::Ordering::Greater => right = middle,
                std::cmp::Ordering::Equal => return Ok(Some(self.assignments[middle].1)),
            }
        }
        Ok(None)
    }

    pub(crate) fn internal_target<'a>(
        &'a self,
        inline: &Inline,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<Option<&'a str>, ConversionError> {
        let Some(index) = self.assignment(inline, budget)? else { return Ok(None) };
        let target = &self.candidates[index].target;
        Ok(target.starts_with("#pdf-page-").then_some(target.as_str()))
    }
}

fn covered_keys(
    left: &[SourceKey],
    right: &[SourceKey],
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if left.len() > right.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        budget.compare()?;
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => return Ok(false),
            std::cmp::Ordering::Equal => {
                a += 1;
                b += 1;
            }
            std::cmp::Ordering::Greater => b += 1,
        }
    }
    Ok(a == left.len())
}

fn match_characters(
    candidate: &mut Candidate,
    lines: &[Line],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let Some(bounds) = candidate.bounds else { return Ok(()) };
    for line in lines {
        budget.compare()?;
        if line.source_kind != SourceKind::Native || intersection_area(bounds, line.bounds) == 0.0 {
            continue;
        }
        for atom in &line.atoms {
            budget.compare()?;
            let r = atom.bounds;
            let center = (r.x + r.width / 2.0, r.y + r.height / 2.0);
            if center.0 >= bounds.x
                && center.0 <= bounds.x + bounds.width
                && center.1 >= bounds.y
                && center.1 <= bounds.y + bounds.height
                && intersection_area(bounds, r) >= r.width * r.height * 0.5
                && let Some(key) = SourceKey::of(&atom.inline)
            {
                budget.push(&mut candidate.keys, key)?;
            }
        }
    }
    Ok(())
}

fn intersect(
    left: &[SourceKey],
    right: &[SourceKey],
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        budget.compare()?;
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => return Ok(true),
        }
    }
    Ok(false)
}
