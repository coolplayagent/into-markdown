use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, ResourceReservation,
};

use super::ole::CompoundBudget;

/// Aggregate limits shared by every layer of one MSG conversion.
pub(super) struct MsgBudget<'a> {
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    entries: u32,
    expanded_bytes: u64,
    asset_bytes: u64,
    work: u64,
}

impl<'a> MsgBudget<'a> {
    pub(super) fn new(
        input_bytes: usize,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let input_bytes = u64::try_from(input_bytes).unwrap_or(u64::MAX);
        if input_bytes > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!(
                    "MSG source has {input_bytes} bytes; limit is {}",
                    options.limits.max_input_bytes
                ),
            ));
        }
        context.checkpoint()?;
        Ok(Self { options, context, entries: 0, expanded_bytes: 0, asset_bytes: 0, work: 0 })
    }

    pub(super) fn entry(&mut self) -> Result<(), ConversionError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| limit("max_archive_entries", "MSG entry count overflowed"))?;
        if self.entries > self.options.limits.max_archive_entries {
            return Err(limit(
                "max_archive_entries",
                format!(
                    "MSG has more than {} directory/property/attachment entries",
                    self.options.limits.max_archive_entries
                ),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn expanded(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.expanded_bytes = self
            .expanded_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("max_decompressed_bytes", "MSG expanded byte count overflowed"))?;
        if self.expanded_bytes > self.options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                format!(
                    "MSG streams retain {} bytes; limit is {}",
                    self.expanded_bytes, self.options.limits.max_decompressed_bytes
                ),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn asset(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if bytes > self.options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!(
                    "MSG attachment has {bytes} bytes; limit is {}",
                    self.options.limits.max_asset_bytes
                ),
            ));
        }
        self.asset_bytes = self.asset_bytes.checked_add(bytes).ok_or_else(|| {
            limit("max_total_asset_bytes", "MSG attachment byte count overflowed")
        })?;
        if self.asset_bytes > self.options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!(
                    "MSG attachments retain {} bytes; limit is {}",
                    self.asset_bytes, self.options.limits.max_total_asset_bytes
                ),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn depth(&self, depth: u16, part: &str) -> Result<(), ConversionError> {
        if depth > self.options.limits.max_nesting_depth {
            return Err(limit(
                "max_nesting_depth",
                format!(
                    "MSG nesting at {part} reached {depth}; limit is {}",
                    self.options.limits.max_nesting_depth
                ),
            ));
        }
        self.context.checkpoint()
    }

    pub(super) fn work(&mut self, units: u64) -> Result<(), ConversionError> {
        let maximum = u64::from(self.options.limits.max_archive_entries.max(1));
        let maximum = maximum
            .checked_mul(4096)
            .and_then(|value| value.checked_add(self.options.limits.max_input_bytes))
            .unwrap_or(u64::MAX);
        self.work = self
            .work
            .checked_add(units)
            .ok_or_else(|| limit("msg_work", "MSG parser work count overflowed"))?;
        if self.work > maximum {
            return Err(limit("msg_work", format!("MSG parser work exceeds {maximum} units")));
        }
        self.checkpoint()
    }

    pub(super) fn checkpoint(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(super) const fn options(&self) -> &ConversionOptions {
        self.options
    }

    pub(super) const fn context(&self) -> &ExecutionContext {
        self.context
    }
}

impl CompoundBudget for MsgBudget<'_> {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.context.reserve_memory(bytes)
    }

    fn cfb_entry(&mut self) -> Result<(), ConversionError> {
        self.entry()
    }

    fn cfb_expanded(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.expanded(bytes)
    }

    fn cfb_depth(&self, depth: u16, part: &str) -> Result<(), ConversionError> {
        self.depth(depth, part)
    }

    fn cfb_work(&mut self, units: u64) -> Result<(), ConversionError> {
        self.work(units)
    }
}

pub(super) fn malformed(part: impl Into<String>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}
