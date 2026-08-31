use crate::text::LogicalMemory;
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, MAX_DOCUMENT_NODES,
};

pub(super) struct Budget<'a> {
    pub options: &'a ConversionOptions,
    pub context: &'a ExecutionContext,
    pub events: usize,
    pub cells: usize,
    pub expanded: u64,
}

impl<'a> Budget<'a> {
    pub fn new(options: &'a ConversionOptions, context: &'a ExecutionContext) -> Self {
        Self { options, context, events: 0, cells: 0, expanded: 0 }
    }

    pub fn event(&mut self) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.events += 1;
        if self.events > 1_000_000 {
            return Err(limit("drawio_xml_events", "XML work exceeds 1000000 events/attributes"));
        }
        Ok(())
    }

    pub fn cell(&mut self) -> Result<(), ConversionError> {
        self.cells += 1;
        if self.cells > MAX_DOCUMENT_NODES {
            return Err(limit("drawio_cells", "diagram cell count exceeds document node budget"));
        }
        self.context.checkpoint()
    }

    pub fn field(&self, bytes: usize) -> Result<(), ConversionError> {
        if bytes as u64 > self.options.limits.max_field_bytes {
            return Err(limit("max_field_bytes", "Drawio XML token or decoded field is too large"));
        }
        Ok(())
    }

    pub fn expand(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.expanded = self
            .expanded
            .checked_add(bytes as u64)
            .ok_or_else(|| limit("max_decompressed_bytes", "decoded size overflow"))?;
        if self.expanded > self.options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                "Drawio decoded bytes exceed request limit",
            ));
        }
        self.context.checkpoint()
    }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

pub(super) fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("drawio".into()), detail: detail.into() }
}

pub(super) fn owned(value: &str, memory: &mut LogicalMemory) -> Result<String, ConversionError> {
    memory.charge(value.len())?;
    Ok(value.to_owned())
}

pub(super) fn size(value: u64) -> Result<usize, ConversionError> {
    usize::try_from(value)
        .map_err(|_| limit("max_memory_bytes", "Drawio size exceeds addressable memory"))
}
