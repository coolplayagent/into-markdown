use super::{Links, whitespace};
use crate::budget::LayoutBudget;
use into_markdown_core::{Block, BlockNode, ConversionError, Inline};

impl Links {
    pub(crate) fn attach(
        &mut self,
        nodes: &mut [BlockNode],
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        for node in nodes {
            budget.checkpoint_item()?;
            match &mut node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => {
                    self.attach_inlines(values, budget)?;
                }
                Block::List { items, .. } => {
                    for item in items {
                        self.attach(&mut item.blocks, budget)?;
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
                        self.attach(&mut cell.blocks, budget)?;
                    }
                }
                Block::Footnote { blocks, .. } => self.attach(blocks, budget)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn attach_inlines(
        &mut self,
        values: &mut Vec<Inline>,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        if self.assignments.is_empty() {
            return Ok(());
        }
        let mut output = Vec::new();
        let mut run = Vec::new();
        let mut spaces = Vec::new();
        let mut active = None;
        for inline in std::mem::take(values) {
            budget.checkpoint_item()?;
            if whitespace(&inline) {
                budget.push(&mut spaces, inline)?;
                continue;
            }
            let next = self.assignment(&inline, budget)?;
            if next == active {
                for space in spaces.drain(..) {
                    budget.push(&mut run, space)?;
                }
            } else {
                self.flush(&mut output, &mut run, active, budget)?;
                for space in spaces.drain(..) {
                    budget.push(&mut output, space)?;
                }
                active = next;
            }
            budget.push(&mut run, inline)?;
        }
        self.flush(&mut output, &mut run, active, budget)?;
        for space in spaces {
            budget.push(&mut output, space)?;
        }
        *values = output;
        Ok(())
    }

    fn flush(
        &mut self,
        output: &mut Vec<Inline>,
        run: &mut Vec<Inline>,
        active: Option<usize>,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        if run.is_empty() {
            return Ok(());
        }
        if let Some(index) = active {
            budget.generated_link()?;
            let target = budget.copy_string(&self.candidates[index].target)?;
            self.candidates[index].used = true;
            budget.push(output, Inline::Link { target, content: std::mem::take(run) })?;
        } else {
            for inline in run.drain(..) {
                budget.push(output, inline)?;
            }
        }
        Ok(())
    }
}
