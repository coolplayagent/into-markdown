//! Terminal accounting for source images selected for an OCR transaction.
use into_markdown_core::ExecutionContext;

pub(super) struct ImageUsage<'a> {
    context: &'a ExecutionContext,
    remaining: u64,
}

impl<'a> ImageUsage<'a> {
    pub(super) fn for_output(
        context: &'a ExecutionContext,
        output: &into_markdown_core::ConverterOutput,
    ) -> Self {
        Self::new(
            context,
            output.assets.iter().filter(|asset| asset.media_type.starts_with("image/")).count()
                as u64,
        )
    }

    pub(super) fn result(&mut self, sources: u64, contribution: &super::CachedContribution) {
        if contribution.recognition_completed {
            self.completed(sources, !contribution.nodes.is_empty());
        } else {
            self.failed(sources);
        }
    }

    pub(super) fn new(context: &'a ExecutionContext, sources: u64) -> Self {
        Self { context, remaining: sources }
    }

    pub(super) fn completed(&mut self, sources: u64, has_text: bool) {
        self.remaining = self.remaining.saturating_sub(sources);
        self.context.record_ocr_images(sources, if has_text { sources } else { 0 }, 0, 0);
    }

    pub(super) fn failed(&mut self, sources: u64) {
        self.remaining = self.remaining.saturating_sub(sources);
        self.context.record_ocr_images(0, 0, sources, 0);
    }
}

impl Drop for ImageUsage<'_> {
    fn drop(&mut self) {
        self.context.record_ocr_images(0, 0, 0, self.remaining);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    #[test]
    fn empty_results_failures_and_early_exit_remain_distinguishable() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        {
            let mut usage = ImageUsage::new(&context, 7);
            usage.completed(2, true);
            usage.completed(1, false);
            usage.failed(1);
        }
        let usage = context.ocr_runtime_usage();
        assert_eq!(usage.image_sources, 7);
        assert_eq!(usage.images_attempted, 4);
        assert_eq!(usage.images_completed, 3);
        assert_eq!(usage.images_with_text, 2);
        assert_eq!(usage.images_failed, 1);
        assert_eq!(usage.images_skipped, 3);
        assert_eq!(usage.requests, 0);
    }
}
