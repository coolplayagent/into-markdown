use super::budget::limit;
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput, Diagnostic,
    DiagnosticSeverity, Document, ErrorPolicy, ExecutionContext, Inline, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, NodeId, Provenance, ProvenanceKind, ResourceReservation, SourceLocator,
};

pub(super) struct Output<'a> {
    pub document: Document,
    pub diagnostics: Vec<Diagnostic>,
    pub options: &'a ConversionOptions,
    pub context: &'a ExecutionContext,
    pub memory: ResourceReservation,
    charged: u64,
    pub nodes: usize,
    pub inlines: usize,
}

impl<'a> Output<'a> {
    pub fn new(
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Ok(Self {
            document: Document::default(),
            diagnostics: Vec::new(),
            options,
            context,
            memory: context.reserve_memory(4096)?,
            charged: 4096,
            nodes: 0,
            inlines: 0,
        })
    }
    pub fn mark(&self) -> (u64, usize, usize, usize) {
        (self.charged, self.nodes, self.inlines, self.diagnostics.len())
    }
    pub fn rewind(&mut self, mark: (u64, usize, usize, usize)) -> Result<(), ConversionError> {
        self.memory.shrink(self.charged.saturating_sub(mark.0))?;
        self.charged = mark.0;
        self.nodes = mark.1;
        self.inlines = mark.2;
        self.diagnostics.truncate(mark.3);
        Ok(())
    }
    pub fn charge(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.memory.grow(bytes as u64)?;
        self.charged += bytes as u64;
        Ok(())
    }
    pub fn inline(&mut self, value: &str) -> Result<Inline, ConversionError> {
        self.inlines += 1;
        if self.inlines > MAX_DOCUMENT_INLINES {
            return Err(limit("documentInlines", "Drawio output exceeds inline budget"));
        }
        self.charge(value.len() + 256)?;
        Ok(Inline::Text { value: value.to_owned(), marks: Vec::new() })
    }
    pub fn block(
        &mut self,
        block: Block,
        locator: &SourceLocator,
    ) -> Result<BlockNode, ConversionError> {
        self.nodes += 1;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(limit("documentNodes", "Drawio output exceeds document node budget"));
        }
        self.charge(2048)?;
        Ok(BlockNode {
            id: NodeId(format!("drawio-{}", self.nodes)),
            block,
            provenance: provenance(locator),
        })
    }
    pub fn paragraph(
        &mut self,
        value: &str,
        locator: &SourceLocator,
    ) -> Result<BlockNode, ConversionError> {
        let text = self.inline(value)?;
        self.block(Block::Paragraph(vec![text]), locator)
    }
    pub fn heading(
        &mut self,
        level: u8,
        value: &str,
        locator: &SourceLocator,
    ) -> Result<BlockNode, ConversionError> {
        let text = self.inline(value)?;
        self.block(Block::Heading { level, content: vec![text] }, locator)
    }
    pub fn defect(
        &mut self,
        code: &str,
        message: String,
        locator: &SourceLocator,
    ) -> Result<(), ConversionError> {
        if self.options.error_policy == ErrorPolicy::Strict {
            return Err(ConversionError::Malformed { part: locator.part.clone(), detail: message });
        }
        self.warning(code, message, locator)
    }
    pub fn warning(
        &mut self,
        code: &str,
        message: String,
        locator: &SourceLocator,
    ) -> Result<(), ConversionError> {
        if self.diagnostics.len() >= MAX_DOCUMENT_NODES {
            return Err(limit("drawio_diagnostics", "Drawio diagnostic budget exceeded"));
        }
        self.charge(message.len() + code.len() + 1024)?;
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message,
            locator: Some(locator.clone()),
        });
        Ok(())
    }
    pub fn finish(self) -> Result<ConverterOutput, ConversionError> {
        self.context.checkpoint()?;
        ConverterOutput::new_with_memory_reservation(
            self.document,
            Vec::new(),
            self.diagnostics,
            self.context,
            self.memory,
        )
    }
}

pub(super) fn provenance(locator: &SourceLocator) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: super::PROVIDER.into(),
        locator: locator.clone(),
        confidence: None,
    }
}
