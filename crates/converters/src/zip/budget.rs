use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

/// Counters shared by every archive in one recursive ZIP tree.
pub(super) struct ArchiveBudget<'a> {
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    entries: u64,
    expanded: u64,
}

impl<'a> ArchiveBudget<'a> {
    pub(super) fn new(options: &'a ConversionOptions, context: &'a ExecutionContext) -> Self {
        Self { options, context, entries: 0, expanded: 0 }
    }

    pub(super) fn enter_archive(
        &mut self,
        depth: u16,
        entries: usize,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        if depth == 0 || depth > self.options.limits.max_archive_depth {
            return Err(limit(
                "max_archive_depth",
                format!("archive depth {depth} exceeds {}", self.options.limits.max_archive_depth),
            ));
        }
        let entries = u64::try_from(entries)
            .map_err(|_| limit("max_archive_entries", "archive entry count overflowed"))?;
        self.entries = self
            .entries
            .checked_add(entries)
            .ok_or_else(|| limit("max_archive_entries", "archive entry count overflowed"))?;
        if self.entries > u64::from(self.options.limits.max_archive_entries) {
            return Err(limit(
                "max_archive_entries",
                format!(
                    "recursive archive tree contains {} entries, maximum is {}",
                    self.entries, self.options.limits.max_archive_entries
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_member(
        &self,
        name: &str,
        compressed: u64,
        expanded: u64,
    ) -> Result<(), ConversionError> {
        if expanded > self.options.limits.max_archive_entry_bytes {
            return Err(limit(
                "max_archive_entry_bytes",
                format!(
                    "archive member {name} expands to {expanded} bytes, maximum is {}",
                    self.options.limits.max_archive_entry_bytes
                ),
            ));
        }
        let maximum = u128::from(compressed)
            .checked_mul(u128::from(self.options.limits.max_archive_compression_ratio))
            .ok_or_else(|| {
                limit("max_archive_compression_ratio", "compression-ratio budget overflowed")
            })?;
        if expanded > 0 && (compressed == 0 || u128::from(expanded) > maximum) {
            return Err(limit(
                "max_archive_compression_ratio",
                format!(
                    "archive member {name} declares ratio {expanded}:{compressed}, maximum is {}:1",
                    self.options.limits.max_archive_compression_ratio
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn charge_expanded(
        &mut self,
        name: &str,
        bytes: u64,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.expanded = self.expanded.checked_add(bytes).ok_or_else(|| {
            limit("max_decompressed_bytes", "recursive decompressed byte count overflowed")
        })?;
        if self.expanded > self.options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                format!(
                    "extracting {name} raises recursive total to {}, maximum is {}",
                    self.expanded, self.options.limits.max_decompressed_bytes
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn context(&self) -> &'a ExecutionContext {
        self.context
    }

    pub(super) fn zip_charset(&self) -> Option<&str> {
        self.options.archive.zip_charset.as_deref()
    }
}

fn limit(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}
