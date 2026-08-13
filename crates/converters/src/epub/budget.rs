//! Request-wide EPUB parser counters.

use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

const MAX_XML_EVENTS: u64 = 1_000_000;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 4096;
const CHECKPOINT_INTERVAL: u64 = 1024;

pub(super) struct EpubBudget<'a> {
    context: &'a ExecutionContext,
    events: u64,
    max_depth: usize,
    max_items: usize,
    max_field_bytes: usize,
}

impl<'a> EpubBudget<'a> {
    pub(super) fn new(options: &ConversionOptions, context: &'a ExecutionContext) -> Self {
        Self {
            context,
            events: 0,
            max_depth: usize::from(options.limits.max_nesting_depth),
            max_items: usize::try_from(options.limits.max_archive_entries).unwrap_or(usize::MAX),
            max_field_bytes: usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX),
        }
    }

    pub(super) fn event(&mut self, depth: usize) -> Result<(), ConversionError> {
        self.events = self.events.checked_add(1).ok_or_else(|| limit("epub_xml_events"))?;
        if self.events > MAX_XML_EVENTS {
            return Err(limit("epub_xml_events"));
        }
        if depth > self.max_depth {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: format!("EPUB XML depth {depth} exceeds {}", self.max_depth),
            });
        }
        if self.events.is_multiple_of(CHECKPOINT_INTERVAL) {
            self.context.checkpoint()?;
        }
        Ok(())
    }

    pub(super) fn attributes(count: usize) -> Result<(), ConversionError> {
        if count > MAX_ATTRIBUTES_PER_ELEMENT {
            return Err(ConversionError::ResourceLimit {
                limit: "epub_xml_attributes",
                detail: format!(
                    "EPUB XML element has {count} attributes, maximum is {MAX_ATTRIBUTES_PER_ELEMENT}"
                ),
            });
        }
        Ok(())
    }

    pub(super) fn items(&self, label: &'static str, count: usize) -> Result<(), ConversionError> {
        if count > self.max_items {
            return Err(ConversionError::ResourceLimit {
                limit: "max_archive_entries",
                detail: format!("EPUB {label} count {count} exceeds {}", self.max_items),
            });
        }
        Ok(())
    }

    pub(super) fn checkpoint(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(super) fn field(&self, label: &'static str, bytes: usize) -> Result<(), ConversionError> {
        if bytes > self.max_field_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: format!("EPUB {label} exceeds {} bytes", self.max_field_bytes),
            });
        }
        Ok(())
    }
}

fn limit(limit_name: &'static str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: limit_name,
        detail: "EPUB XML event budget exceeded".into(),
    }
}
