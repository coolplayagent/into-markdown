use super::{Association, Candidate, Links, SourceKey, source_link};
use crate::budget::LayoutBudget;
use crate::{LINK_SOURCE_PART, collect, memory};
use into_markdown_core::{Block, BlockNode, ConversionError, Inline, NodeId, Provenance};

impl Links {
    pub(crate) fn extract(
        page: u32,
        width: f32,
        height: f32,
        blocks: &mut Vec<BlockNode>,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<Self, ConversionError> {
        let mut links = Self::default();
        let mut retained = budget.reserve(blocks.len())?;
        for mut node in std::mem::take(blocks) {
            budget.checkpoint_item()?;
            if let Some(target) = spatial_target(&node) {
                let target = budget.copy_string(target)?;
                budget.push(
                    &mut links.candidates,
                    Candidate {
                        target,
                        bounds: node.provenance.locator.bounds,
                        keys: Vec::new(),
                        fallback: node,
                        spatial: true,
                        state: Association::Ready,
                        used: false,
                    },
                )?;
            } else {
                if collect::block_is_collectable(&node, page, width, height)? {
                    links.detach(&mut node, page, budget)?;
                }
                retained.push(node);
            }
        }
        *blocks = retained;
        Ok(links)
    }

    fn detach(
        &mut self,
        node: &mut BlockNode,
        page: u32,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        budget.checkpoint_item()?;
        match &mut node.block {
            Block::Paragraph(values)
            | Block::Heading { content: values, .. }
            | Block::TimedSegment { content: values, .. } => {
                self.detach_inlines(values, &node.provenance, page, budget)?;
            }
            Block::List { items, .. } => {
                for child in items.iter_mut().flat_map(|item| &mut item.blocks) {
                    self.detach(child, page, budget)?;
                }
            }
            Block::Table { rows, .. } => {
                for child in
                    rows.iter_mut().flat_map(|row| &mut row.cells).flat_map(|cell| &mut cell.blocks)
                {
                    self.detach(child, page, budget)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn detach_inlines(
        &mut self,
        values: &mut Vec<Inline>,
        provenance: &Provenance,
        page: u32,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        if !values.iter().any(|value| matches!(value, Inline::Link { .. })) {
            return Ok(());
        }
        let mut flattened = Vec::new();
        for inline in std::mem::take(values) {
            if matches!(&inline, Inline::Link { content, .. } if source_link(content)) {
                let Inline::Link { target, content } = inline else { unreachable!() };
                let mut keys = budget.reserve(content.len())?;
                let mut bounds = None;
                for inline in content {
                    budget.checkpoint_item()?;
                    if let Some(key) = SourceKey::of(&inline) {
                        keys.push(key);
                        let r = into_markdown_core::Rect {
                            x: f32::from_bits(key.bounds[0]),
                            y: f32::from_bits(key.bounds[1]),
                            width: f32::from_bits(key.bounds[2]),
                            height: f32::from_bits(key.bounds[3]),
                        };
                        bounds = Some(bounds.map_or(r, |prior| crate::geometry::union(prior, r)));
                    }
                    budget.push(&mut flattened, inline)?;
                }
                let label = budget.copy_string(&target)?;
                budget.generated_node()?;
                let fallback_target = budget.copy_string(&target)?;
                let mut source = crate::model::block_provenance(
                    page,
                    bounds.unwrap_or_default(),
                    provenance.locator.page_width.unwrap_or_default(),
                    provenance.locator.page_height.unwrap_or_default(),
                    provenance.confidence,
                );
                source.locator.bounds = bounds;
                source.locator.part = Some(LINK_SOURCE_PART.into());
                let mut label_inlines = budget.reserve(1)?;
                label_inlines.push(Inline::Text { value: label, marks: Vec::new() });
                let mut fallback_inlines = budget.reserve(1)?;
                fallback_inlines
                    .push(Inline::Link { target: fallback_target, content: label_inlines });
                let fallback = BlockNode {
                    id: NodeId(format!("pdf-page-{page}-restored-link-{}", self.candidates.len())),
                    block: Block::Paragraph(fallback_inlines),
                    provenance: source,
                };
                budget.push(
                    &mut self.candidates,
                    Candidate {
                        target,
                        bounds,
                        keys,
                        fallback,
                        spatial: false,
                        state: Association::Ready,
                        used: false,
                    },
                )?;
            } else {
                budget.push(&mut flattened, inline)?;
            }
        }
        *values = flattened;
        Ok(())
    }

    pub(crate) fn restore_unmatched(
        self,
        blocks: &mut Vec<BlockNode>,
        budget: &mut LayoutBudget<'_>,
    ) -> Result<(), ConversionError> {
        for candidate in self.candidates {
            budget.checkpoint_item()?;
            if !candidate.used && candidate.state != Association::Duplicate {
                blocks.try_reserve(1).map_err(|_| memory("layout unmatched link"))?;
                blocks.push(candidate.fallback);
            }
        }
        Ok(())
    }
}

fn spatial_target(node: &BlockNode) -> Option<&str> {
    if node.provenance.locator.part.as_deref() != Some(LINK_SOURCE_PART) {
        return None;
    }
    let Block::Paragraph(values) = &node.block else { return None };
    let [Inline::Link { target, .. }] = values.as_slice() else { return None };
    Some(target)
}
