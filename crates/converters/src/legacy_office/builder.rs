use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConverterOutput, Diagnostic, DiagnosticSeverity, Document,
    Inline, NodeId, Provenance, ProvenanceKind, SourceLocator,
};

pub(super) const PROVIDER_ID: &str = "builtin.converter.legacy-office";

pub(super) struct OutputBuilder {
    document: Document,
    assets: Vec<Asset>,
    diagnostics: Vec<Diagnostic>,
    next_node: u64,
}

impl OutputBuilder {
    pub(super) fn new(family: &str) -> Self {
        let mut document = Document::default();
        document.metadata.properties.insert("legacyOffice.family".into(), family.into());
        document
            .metadata
            .properties
            .insert("legacyOffice.parser".into(), "into-markdown-native".into());
        Self { document, assets: Vec::new(), diagnostics: Vec::new(), next_node: 1 }
    }

    pub(super) fn node(&mut self, block: Block, locator: SourceLocator) -> BlockNode {
        let id = NodeId(format!("legacy-{}", self.next_node));
        self.next_node += 1;
        BlockNode {
            id,
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator,
                confidence: None,
            },
        }
    }

    pub(super) fn push(&mut self, block: Block, locator: SourceLocator) {
        let node = self.node(block, locator);
        self.document.blocks.push(node);
    }

    pub(super) fn text(value: impl Into<String>) -> Inline {
        Inline::Text { value: value.into(), marks: Vec::new() }
    }

    pub(super) fn warning(
        &mut self,
        code: &str,
        message: impl Into<String>,
        locator: Option<SourceLocator>,
    ) {
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            locator,
        });
    }

    pub(super) fn asset(&mut self, origin: &str, media_type: &str, bytes: Vec<u8>) -> AssetId {
        let id = AssetId(format!("legacy-asset-{}", self.assets.len() + 1));
        let filename =
            origin.rsplit('/').next().filter(|value| !value.is_empty()).map(str::to_owned);
        self.assets.push(Asset {
            id: id.clone(),
            filename,
            media_type: media_type.into(),
            bytes,
            external_uri: None,
        });
        id
    }

    pub(super) fn finish(self) -> ConverterOutput {
        ConverterOutput::new(self.document, self.assets, self.diagnostics)
    }
}

pub(super) fn locator(part: &str) -> SourceLocator {
    SourceLocator { part: Some(part.into()), ..SourceLocator::default() }
}

pub(super) fn slide_locator(part: &str, slide: u32) -> SourceLocator {
    SourceLocator { slide: Some(slide), part: Some(part.into()), ..SourceLocator::default() }
}
