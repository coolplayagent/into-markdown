use into_markdown_core::{ConversionError, ExecutionContext};

const CHECKPOINT_BYTES: usize = 4 * 1024;

pub(crate) struct Meter<'a> {
    context: &'a ExecutionContext,
    remaining: usize,
}

impl<'a> Meter<'a> {
    pub(crate) const fn new(context: &'a ExecutionContext) -> Self {
        Self { context, remaining: CHECKPOINT_BYTES }
    }

    pub(crate) fn consume(&mut self, mut bytes: usize) -> Result<(), ConversionError> {
        while bytes >= self.remaining {
            bytes -= self.remaining;
            self.remaining = CHECKPOINT_BYTES;
            self.context.checkpoint()?;
        }
        self.remaining -= bytes;
        Ok(())
    }

    pub(crate) const fn next_batch(&self) -> usize {
        self.remaining
    }
}
