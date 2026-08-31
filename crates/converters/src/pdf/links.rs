//! Page-local link output and diagnostics, with request-owned allocation leases.
use super::{
    Block, BlockNode, ConversionError, ConversionOptions, Diagnostic, DiagnosticSeverity,
    ErrorPolicy, ExecutionContext, Inline, LinkTarget, NodeId, PageInfo, ResourceReservation,
    diagnostic_overhead, materialize_after_reserve, normalize_rect, output_block_overhead,
    page_locator, pages, provenance, request_path_scan, resource, retain_output_bytes,
    safe_link_target,
};
use into_markdown_pdfium::{Link, LinkIdentity, LinkPolicy, TextPage};

pub(super) struct PageLinks<'a> {
    pub number: u32,
    pub info: &'a PageInfo,
    pub options: &'a ConversionOptions,
    pub context: &'a ExecutionContext,
    pub counts: &'a mut pages::Counts,
    pub blocks: &'a mut Vec<BlockNode>,
    pub diagnostics: &'a mut Vec<Diagnostic>,
    pub retained: &'a mut Vec<ResourceReservation>,
}

impl PageLinks<'_> {
    pub(super) fn extract(
        &mut self,
        text: &TextPage<'_>,
        page_count: u32,
    ) -> Result<(), ConversionError> {
        let policy = match self.options.error_policy {
            ErrorPolicy::Strict => LinkPolicy::Strict,
            ErrorPolicy::BestEffort => LinkPolicy::BestEffort,
        };
        let plan =
            request_path_scan(self.context, |check| text.plan_link_extraction(policy, check))
                .map_err(|error| self.locate(error))?;
        let (extraction, _memory) =
            materialize_after_reserve(self.context, plan.allocation_bytes(), || {
                request_path_scan(self.context, |check| plan.materialize_with_checkpoint(check))
                    .map_err(|error| self.locate(error))
            })?;
        for diagnostic in extraction.diagnostics {
            self.omitted(diagnostic.identity, diagnostic.reason)?;
        }
        for (index, link) in extraction.links.into_iter().enumerate() {
            self.append(index, link, page_count)?;
        }
        Ok(())
    }

    fn locate(&self, error: ConversionError) -> ConversionError {
        match error {
            ConversionError::Malformed { part, detail } => ConversionError::Malformed {
                part,
                detail: format!("page {}: {detail}", self.number),
            },
            other => other,
        }
    }

    fn omitted(
        &mut self,
        identity: LinkIdentity,
        reason: impl std::fmt::Display,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        retain_output_bytes(self.context, self.retained, diagnostic_overhead()?)?;
        self.diagnostics
            .try_reserve(1)
            .map_err(|_| resource("max_memory_bytes", "PDF link diagnostic allocation failed"))?;
        self.diagnostics.push(Diagnostic {
            code: "pdf.linkOmitted".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!("page {}: {identity}: {reason}", self.number),
            locator: Some(page_locator(self.number, self.info)),
        });
        Ok(())
    }

    fn append(&mut self, index: usize, link: Link, page_count: u32) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.counts.account_link()?;
        let length = match &link.target {
            LinkTarget::ExternalUri(value) => value.len(),
            LinkTarget::InternalPage { .. } => 32,
        };
        let bytes = u64::try_from(length)
            .unwrap_or(u64::MAX)
            .checked_mul(3)
            .and_then(|n| n.checked_add(output_block_overhead(2).ok()?))
            .ok_or_else(|| resource("max_memory_bytes", "link IR memory overflow"))?;
        retain_output_bytes(self.context, self.retained, bytes)?;
        let display = match &link.target {
            LinkTarget::ExternalUri(value) => Some(value.clone()),
            LinkTarget::InternalPage { .. } => None,
        };
        let inline = match safe_link_target(link.target, page_count) {
            Ok(target) => Inline::Link {
                content: vec![Inline::Text { value: target.clone(), marks: Vec::new() }],
                target,
            },
            Err(error) if self.options.error_policy == ErrorPolicy::Strict => {
                return Err(self.locate(match error {
                    ConversionError::Malformed { part, detail } => ConversionError::Malformed {
                        part,
                        detail: format!("{}: {detail}", link.identity),
                    },
                    other => other,
                }));
            }
            Err(error) => {
                self.omitted(link.identity, error)?;
                let Some(value) = display else { return Ok(()) };
                Inline::Text { value, marks: Vec::new() }
            }
        };
        self.blocks
            .try_reserve(1)
            .map_err(|_| resource("max_memory_bytes", "PDF link block allocation failed"))?;
        self.blocks.push(BlockNode {
            id: NodeId(format!("pdf-page-{}-link-{index}", self.number)),
            block: Block::Paragraph(vec![inline]),
            provenance: provenance(
                self.number,
                Some(normalize_rect(link.bounds, self.info)?),
                None,
                self.info,
            )?,
        });
        Ok(())
    }
}
