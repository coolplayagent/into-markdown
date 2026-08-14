//! Accounted, non-overlapping encoded-range ownership.

use super::meter::Meter;
use super::{limit, malformed};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use std::collections::BTreeMap;

const ACCOUNTED_BYTES_PER_INTERVAL: u64 = 128;

pub(super) struct IntervalGraph<'a> {
    ranges: BTreeMap<usize, usize>,
    memory: ResourceReservation,
    context: &'a ExecutionContext,
    max_intervals: u64,
}

impl<'a> IntervalGraph<'a> {
    pub(super) fn new(
        max_intervals: u64,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Ok(Self {
            ranges: BTreeMap::new(),
            memory: context.reserve_memory(0)?,
            context,
            max_intervals,
        })
    }

    pub(super) fn add(
        &mut self,
        start: usize,
        end: usize,
        source_len: usize,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        if start >= end || end > source_len {
            return Err(malformed("declared image interval is empty or outside the file"));
        }
        if self.ranges.len() as u64 >= self.max_intervals {
            return Err(limit("image_intervals", "image has too many declared intervals"));
        }
        if self
            .ranges
            .range(..=start)
            .next_back()
            .is_some_and(|(_, previous_end)| *previous_end > start)
            || self.ranges.range(start..).next().is_some_and(|(next_start, _)| *next_start < end)
        {
            return Err(malformed("declared image intervals overlap or alias"));
        }
        self.memory.grow(ACCOUNTED_BYTES_PER_INTERVAL)?;
        if self.ranges.insert(start, end).is_some() {
            return Err(malformed("duplicate image interval"));
        }
        Ok(())
    }

    pub(super) fn require_exact_coverage(
        &self,
        bytes: &[u8],
        alignment: usize,
    ) -> Result<(), ConversionError> {
        let mut cursor = 0_usize;
        let mut meter = Meter::new(self.context);
        for (&start, &end) in &self.ranges {
            if start > cursor {
                validate_alignment_padding(bytes, cursor, start, alignment, &mut meter)?;
            }
            cursor = end;
        }
        if cursor < bytes.len() {
            return Err(malformed("image contains bytes after its last declared interval"));
        }
        if cursor > bytes.len() {
            return Err(malformed("declared image interval exceeds EOF"));
        }
        Ok(())
    }
}

fn validate_alignment_padding(
    bytes: &[u8],
    start: usize,
    end: usize,
    alignment: usize,
    meter: &mut Meter<'_>,
) -> Result<(), ConversionError> {
    if alignment == 0 {
        return Err(malformed("image alignment must be nonzero"));
    }
    let expected_end = start
        .checked_add(alignment.saturating_sub(start % alignment) % alignment)
        .ok_or_else(|| malformed("image alignment padding overflow"))?;
    if end != expected_end
        || bytes.get(start..end).is_none_or(|gap| gap.iter().any(|byte| *byte != 0))
    {
        return Err(malformed(format!(
            "image contains an unowned gap {start}..{end} instead of canonical alignment padding"
        )));
    }
    meter.consume(end - start)
}
