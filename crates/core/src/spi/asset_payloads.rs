use super::ConverterOutput;
use crate::{ConversionError, ExecutionContext, estimate_retained_output};

impl ConverterOutput {
    #[doc(hidden)]
    #[must_use]
    pub fn leased_memory_for(&self, context: &ExecutionContext) -> u64 {
        self.memory_lease.bytes_for(context)
    }

    /// Drop binary payloads that the selected output policy will not publish,
    /// then return their retained request credit before rendering.
    #[doc(hidden)]
    pub fn discard_asset_payloads(
        mut self,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        for asset in &mut self.assets {
            asset.bytes = Vec::new();
        }
        self.reconcile_retained_output(context)
    }

    /// Reconcile the retained lease after a caller has transactionally removed
    /// selected asset payloads while preserving the remaining inventory.
    #[doc(hidden)]
    pub fn reconcile_retained_output(
        mut self,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let required = estimate_retained_output(&self.document, &self.assets, &self.diagnostics)?;
        if self.memory_lease.leases.iter().any(|lease| !lease.belongs_to_memory_context(context)) {
            return Err(ConversionError::Internal {
                detail: "omitted asset payload lease belongs to a different context".into(),
            });
        }
        let mut keep = required;
        for lease in &mut self.memory_lease.leases {
            let retained = lease.bytes().min(keep);
            lease.shrink(lease.bytes() - retained)?;
            keep -= retained;
        }
        if keep != 0 {
            // Legacy/source-compatible converters may return an unaccounted
            // output that the engine normally certifies at the next boundary.
            // Once payload ownership has been dropped, close that metadata
            // deficit directly against the same request context.
            self.memory_lease.push(context.reserve_memory(keep)?)?;
        }
        self.memory_lease.accounted_bytes = required;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::ConverterOutput;
    use crate::{Asset, AssetId, Document, ExecutionContext, ExecutionOptions, ResourceLimits};

    #[test]
    fn omitted_asset_payloads_release_bytes_and_retained_credit() {
        let mut bytes = Vec::with_capacity(1024 * 1024);
        bytes.extend_from_slice(b"payload");
        let output = ConverterOutput::new(
            Document::default(),
            vec![Asset {
                id: AssetId("omitted".into()),
                filename: Some("scan.png".into()),
                media_type: "image/png".into(),
                bytes,
                external_uri: None,
            }],
            Vec::new(),
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let output = output.account_retained(&context).unwrap();
        let before = context.reserved_memory_bytes();
        assert!(before >= 1024 * 1024);

        let output = output.discard_asset_payloads(&context).unwrap();
        assert!(output.assets[0].bytes.is_empty());
        assert!(context.reserved_memory_bytes() < before);
        assert_eq!(context.reserved_memory_bytes(), output.leased_memory_for(&context));
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}
