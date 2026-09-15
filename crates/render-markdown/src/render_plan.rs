//! Allocation-free admission of the same text runs used by inline rendering.

use super::{ConversionError, ExecutionContext, Inline, inline, render_plan_overflow};

pub(super) struct RenderPlan<'a> {
    pub(super) bytes: u64,
    pub(super) units: u64,
    pub(super) visited: usize,
    pub(super) context: &'a ExecutionContext,
}
impl RenderPlan<'_> {
    pub(super) fn add(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(render_plan_overflow)?;
        Ok(())
    }
    pub(super) fn unit(&mut self) -> Result<(), ConversionError> {
        self.units = self.units.checked_add(1).ok_or_else(render_plan_overflow)?;
        self.visited = self.visited.saturating_add(1);
        if self.visited.is_multiple_of(1_024) {
            self.context.checkpoint()?;
        }
        Ok(())
    }
    pub(super) fn fenced_text(
        &mut self,
        value: &str,
        language: Option<&str>,
        depth: usize,
    ) -> Result<u64, ConversionError> {
        let source = u64::try_from(value.len()).map_err(|_| render_plan_overflow())?;
        let newlines = value.bytes().filter(|byte| *byte == b'\n').count() as u64;
        let fence = super::longest_run(value, '`').saturating_add(1).max(3) as u64;
        let info = language.map_or(0, str::len) as u64;
        let output = source
            .checked_add(fence.checked_mul(2).ok_or_else(render_plan_overflow)?)
            .and_then(|n| n.checked_add(info.saturating_mul(3)))
            .and_then(|n| n.checked_add(3))
            .ok_or_else(render_plan_overflow)?;
        // Fences preserve source bytes. Only the info string uses percent encoding.
        self.add(source)?;
        self.add(source)?;
        self.add(fence)?;
        self.add(info.saturating_mul(3))?;
        self.add(output.checked_mul(2).ok_or_else(render_plan_overflow)?)?;
        let mut rendered = output;
        for _ in 0..depth {
            self.add(rendered)?;
            rendered = rendered
                .checked_add(newlines.saturating_mul(4))
                .and_then(|n| n.checked_add(64))
                .ok_or_else(render_plan_overflow)?;
            self.add(rendered)?;
            self.add(rendered)?;
            self.add(rendered)?;
        }
        Ok(output)
    }

    pub(super) fn text(&mut self, value: &str, depth: usize) -> Result<u64, ConversionError> {
        let source = u64::try_from(value.len()).map_err(|_| render_plan_overflow())?;
        let newlines = u64::try_from(value.bytes().filter(|byte| *byte == b'\n').count())
            .map_err(|_| render_plan_overflow())?;
        self.text_size(source, newlines, depth)
    }
    pub(super) fn text_size(
        &mut self,
        source: u64,
        newlines: u64,
        depth: usize,
    ) -> Result<u64, ConversionError> {
        // `normalize_lf` owns two successive replace results; `single_line`
        // owns one more. `escape_text` expands each input byte by at most
        // five bytes (`&amp;`) and marked text can wrap all six supported
        // marks with at most 13 bytes each.
        self.add(source)?;
        self.add(source)?;
        self.add(source)?;
        // Adjacent marked runs own a joined buffer with geometric capacity.
        self.add(source)?;
        self.add(source)?;
        let output = source
            .checked_mul(5)
            .and_then(|value| value.checked_add(6 * 13))
            .ok_or_else(render_plan_overflow)?;
        let mut rendered = output;
        self.add(rendered)?;
        // At each typed block ancestor the child string remains alive while
        // indentation replacement, formatting, the Vec<String> slot, and
        // the container join allocate their own result. This recurrence
        // mirrors those four concrete owners instead of applying a global
        // depth multiplier.
        for _ in 0..depth {
            self.add(rendered)?;
            rendered = rendered
                .checked_add(newlines.checked_mul(4).ok_or_else(render_plan_overflow)?)
                .and_then(|value| value.checked_add(64))
                .ok_or_else(render_plan_overflow)?;
            self.add(rendered)?;
            self.add(rendered)?;
            self.add(rendered)?;
        }
        Ok(output)
    }
}

pub(super) fn plan_inlines(
    values: &[Inline],
    block_depth: usize,
    link_depth: usize,
    plan: &mut RenderPlan<'_>,
) -> Result<u64, ConversionError> {
    if link_depth > 2 {
        return Err(ConversionError::Internal {
            detail: "renderer preflight rejected nested links".into(),
        });
    }
    let mut output = 0_u64;
    let mut index = 0;
    while let Some(value) = values.get(index) {
        plan.unit()?;
        if let Some((_, marks)) = inline::text_parts(value) {
            // Match render_inlines' contiguous equal-mark run, without
            // allocating the joined string during admission planning.
            let mut bytes = 0_u64;
            let mut newlines = 0_u64;
            while let Some((text, next_marks)) = values.get(index).and_then(inline::text_parts) {
                if !inline::same_marks(marks, next_marks) {
                    break;
                }
                bytes = bytes.checked_add(text.len() as u64).ok_or_else(render_plan_overflow)?;
                newlines = newlines
                    .checked_add(text.bytes().filter(|b| *b == b'\n').count() as u64)
                    .ok_or_else(render_plan_overflow)?;
                index += 1;
                if index.is_multiple_of(1024) {
                    plan.context.checkpoint()?;
                }
            }
            output = output
                .checked_add(plan.text_size(bytes, newlines, block_depth)?)
                .ok_or_else(render_plan_overflow)?;
            continue;
        }
        index += 1;
        let rendered = match value {
            Inline::Code(value) | Inline::Formula(value) | Inline::FootnoteReference(value) => {
                plan.text(value, block_depth)?
            }
            Inline::Link { target, content } => {
                let target_output = plan.text(target, block_depth)?;
                plan_inlines(content, block_depth, link_depth + 1, plan)?
                    .checked_add(target_output.checked_mul(3).ok_or_else(render_plan_overflow)?)
                    .and_then(|value| value.checked_add(6))
                    .ok_or_else(render_plan_overflow)?
            }
            Inline::LineBreak => {
                plan.add(4)?;
                4
            }
            _ => {
                return Err(ConversionError::Internal {
                    detail: "renderer preflight encountered an unsupported future inline".into(),
                });
            }
        };
        output = output.checked_add(rendered).ok_or_else(render_plan_overflow)?;
    }
    Ok(output)
}
