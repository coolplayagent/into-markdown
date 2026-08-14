use crate::{LayoutConfig, PagePathEvidence, limit, memory};
use into_markdown_core::{
    Block, BlockNode, ConversionError, Document, ExecutionContext, Inline, ResourceReservation,
};

const CHECKPOINT_ITEMS: usize = 256;
const CHECKPOINT_BYTES: usize = 4 * 1024;
const ATOM_HIGH_WATER: u64 = 1_536;
const NODE_HIGH_WATER: u64 = 2_048;
const PATH_BOUND_HIGH_WATER: u64 = 128;

#[cfg(test)]
type CheckpointHook = Option<Box<dyn FnMut()>>;

#[cfg(test)]
std::thread_local! {
    static CHECKPOINT_HOOK: std::cell::RefCell<CheckpointHook> = std::cell::RefCell::new(None);
}

pub(crate) struct LayoutBudget<'a> {
    context: &'a ExecutionContext,
    reservation: ResourceReservation,
    comparisons: u64,
    max_comparisons: u64,
    items_since_checkpoint: usize,
    bytes_since_checkpoint: usize,
    atoms: usize,
    lines: usize,
    max_atoms: usize,
    max_lines: usize,
}

impl<'a> LayoutBudget<'a> {
    pub(crate) fn preflight(
        document: &Document,
        path_evidence: &[PagePathEvidence],
        config: &LayoutConfig,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        context.checkpoint()?;
        let mut counts = Counts::default();
        let mut stack = Vec::new();
        stack.try_reserve_exact(1).map_err(|_| memory("layout preflight stack"))?;
        stack.push(document.blocks.as_slice());
        while let Some(nodes) = stack.pop() {
            for node in nodes {
                counts.nodes = add(counts.nodes, 1, "node count")?;
                count_node(node, &mut counts, &mut stack)?;
                if counts.nodes.is_multiple_of(CHECKPOINT_ITEMS) {
                    context.checkpoint()?;
                }
            }
        }
        if counts.atoms > config.limits.max_atoms {
            return Err(limit(
                "pdfLayoutAtoms",
                format!("{} > {}", counts.atoms, config.limits.max_atoms),
            ));
        }
        let mut path_bounds = 0_usize;
        for page in path_evidence {
            path_bounds = add(path_bounds, page.bounds.len(), "path bounds count")?;
            if path_bounds.is_multiple_of(CHECKPOINT_ITEMS) {
                context.checkpoint()?;
            }
        }
        if path_bounds > config.limits.max_lines {
            return Err(limit(
                "pdfLayoutPathBounds",
                format!("{path_bounds} > {}", config.limits.max_lines),
            ));
        }
        let bytes = u64::try_from(counts.text_bytes)
            .map_err(|_| memory("layout text byte count"))?
            .checked_mul(4)
            .and_then(|value| {
                value.checked_add(u64::try_from(counts.atoms).ok()?.checked_mul(ATOM_HIGH_WATER)?)
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(counts.nodes).ok()?.checked_mul(NODE_HIGH_WATER)?)
            })
            .and_then(|value| {
                value.checked_add(
                    u64::try_from(path_bounds).ok()?.checked_mul(PATH_BOUND_HIGH_WATER)?,
                )
            })
            .and_then(|value| value.checked_add(64 * 1024))
            .ok_or_else(|| memory("layout working-set overflow"))?;
        let reservation = context.reserve_memory(bytes)?;
        Ok(Self {
            context,
            reservation,
            comparisons: 0,
            max_comparisons: config.limits.max_comparisons,
            items_since_checkpoint: 0,
            bytes_since_checkpoint: 0,
            atoms: 0,
            lines: 0,
            max_atoms: config.limits.max_atoms,
            max_lines: config.limits.max_lines,
        })
    }

    pub(crate) fn consume_atom(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.atoms = add(self.atoms, 1, "atom count")?;
        if self.atoms > self.max_atoms {
            return Err(limit("pdfLayoutAtoms", format!("{} > {}", self.atoms, self.max_atoms)));
        }
        self.checkpoint_bytes(bytes)
    }

    pub(crate) fn consume_line(&mut self) -> Result<(), ConversionError> {
        self.lines = add(self.lines, 1, "line count")?;
        if self.lines > self.max_lines {
            return Err(limit("pdfLayoutLines", format!("{} > {}", self.lines, self.max_lines)));
        }
        self.checkpoint_item()
    }

    pub(crate) fn compare(&mut self) -> Result<(), ConversionError> {
        self.comparisons = self
            .comparisons
            .checked_add(1)
            .ok_or_else(|| limit("pdfLayoutComparisons", "comparison count overflow"))?;
        if self.comparisons > self.max_comparisons {
            return Err(limit(
                "pdfLayoutComparisons",
                format!("{} > {}", self.comparisons, self.max_comparisons),
            ));
        }
        self.checkpoint_item()
    }

    pub(crate) fn checkpoint_item(&mut self) -> Result<(), ConversionError> {
        self.items_since_checkpoint = add(self.items_since_checkpoint, 1, "checkpoint items")?;
        if self.items_since_checkpoint >= CHECKPOINT_ITEMS {
            self.items_since_checkpoint = 0;
            #[cfg(test)]
            CHECKPOINT_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow_mut().as_mut() {
                    hook();
                }
            });
            self.context.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint_now(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(crate) fn checkpoint_bytes(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.bytes_since_checkpoint = add(self.bytes_since_checkpoint, bytes, "checkpoint bytes")?;
        while self.bytes_since_checkpoint >= CHECKPOINT_BYTES {
            self.bytes_since_checkpoint -= CHECKPOINT_BYTES;
            self.context.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ResourceReservation, ConversionError> {
        self.context.checkpoint()?;
        Ok(self.reservation)
    }
}

#[cfg(test)]
pub(crate) fn set_checkpoint_hook(hook: CheckpointHook) {
    CHECKPOINT_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

#[derive(Default)]
struct Counts {
    nodes: usize,
    atoms: usize,
    text_bytes: usize,
}

fn count_node<'a>(
    node: &'a BlockNode,
    counts: &mut Counts,
    stack: &mut Vec<&'a [BlockNode]>,
) -> Result<(), ConversionError> {
    match &node.block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => count_inlines(inlines, counts)?,
        Block::List { items, .. } => {
            stack.try_reserve(items.len()).map_err(|_| memory("layout list stack"))?;
            stack.extend(items.iter().map(|item| item.blocks.as_slice()));
        }
        Block::Table { rows, .. } => {
            let cells = rows.iter().map(|row| row.cells.len()).sum::<usize>();
            stack.try_reserve(cells).map_err(|_| memory("layout table stack"))?;
            stack.extend(rows.iter().flat_map(|row| &row.cells).map(|cell| cell.blocks.as_slice()));
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            stack.try_reserve(1).map_err(|_| memory("layout block stack"))?;
            stack.push(blocks);
        }
        Block::Code { text, .. } | Block::Formula(text) => {
            counts.text_bytes = add(counts.text_bytes, text.len(), "text bytes")?;
        }
        Block::Image { alt: Some(alt), .. } => {
            counts.text_bytes = add(counts.text_bytes, alt.len(), "alt bytes")?;
        }
        _ => {}
    }
    Ok(())
}

fn count_inlines(inlines: &[Inline], counts: &mut Counts) -> Result<(), ConversionError> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(1).map_err(|_| memory("layout inline stack"))?;
    stack.push(inlines);
    while let Some(inlines) = stack.pop() {
        for inline in inlines {
            match inline {
                Inline::SourceText { value, .. } | Inline::OcrText { value, .. } => {
                    counts.atoms = add(counts.atoms, 1, "atom count")?;
                    counts.text_bytes = add(counts.text_bytes, value.len(), "text bytes")?;
                }
                Inline::Text { value, .. } | Inline::Code(value) | Inline::Formula(value) => {
                    counts.text_bytes = add(counts.text_bytes, value.len(), "text bytes")?;
                }
                Inline::Link { target, content } => {
                    counts.text_bytes = add(counts.text_bytes, target.len(), "link bytes")?;
                    stack.try_reserve(1).map_err(|_| memory("layout inline stack"))?;
                    stack.push(content);
                }
                Inline::FootnoteReference(value) => {
                    counts.text_bytes = add(counts.text_bytes, value.len(), "footnote bytes")?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn add(left: usize, right: usize, detail: &'static str) -> Result<usize, ConversionError> {
    left.checked_add(right).ok_or_else(|| memory(detail))
}
