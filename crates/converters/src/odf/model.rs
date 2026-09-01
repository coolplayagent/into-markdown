use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, Diagnostic, DiagnosticSeverity, Document,
    InputFormat, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, NodeId, Provenance, ProvenanceKind,
    SourceLocator,
};
use std::collections::BTreeMap;

pub(super) const PROVIDER_ID: &str = "builtin.converter.odf";
pub(super) const FORMATS: &[InputFormat] = &[InputFormat::Odt, InputFormat::Ods, InputFormat::Odp];
pub(super) const OFFICE_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
pub(super) const TEXT_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
pub(super) const TABLE_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
pub(super) const DRAW_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
pub(super) const PRESENTATION_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:presentation:1.0";
pub(super) const STYLE_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
pub(super) const MANIFEST_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
pub(super) const META_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:meta:1.0";
pub(super) const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
pub(super) const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
pub(super) const SVG_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";
pub(super) const FO_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";
pub(super) const CONFIG_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:config:1.0";
pub(super) const NUMBER_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0";
pub(super) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
pub(super) const MAX_XML_EVENTS: usize = 8_000_000;
pub(super) const ZIP_STREAM_CHUNK: usize = 16 * 1024;
pub(super) const PACKAGE_BASE_WORKING_BYTES: u64 = 1024 * 1024;
pub(super) const IMAGE_DECODER_HEADER_BYTES: u64 = 262_144;

#[derive(Clone, Debug)]
pub(super) struct ListLevelSpec {
    pub(super) ordered: bool,
    pub(super) start: u64,
}

#[derive(Default)]
pub(super) struct ParseState {
    pub(super) document: Document,
    pub(super) assets: Vec<Asset>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) asset_ids: BTreeMap<String, AssetId>,
    pub(super) next_node: usize,
    pub(super) inline_count: usize,
    pub(super) table_rows: u64,
    pub(super) table_cells: u64,
    pub(super) total_asset_bytes: u64,
    pub(super) next_image_anchor: u32,
    pub(super) deferred: Vec<BlockNode>,
    pub(super) list_styles: BTreeMap<String, BTreeMap<u8, ListLevelSpec>>,
    pub(super) list_sequences: BTreeMap<String, (bool, u64)>,
    pub(super) last_list_sequences: BTreeMap<(String, u8), (bool, u64)>,
    pub(super) active_list_styles: Vec<String>,
}

impl ParseState {
    pub(super) fn node(
        &mut self,
        block: Block,
        locator: SourceLocator,
    ) -> Result<BlockNode, ConversionError> {
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| limit("documentNodes", "ODF node count overflow"))?;
        if self.next_node > MAX_DOCUMENT_NODES {
            return Err(limit(
                "documentNodes",
                format!("{} > {MAX_DOCUMENT_NODES}", self.next_node),
            ));
        }
        Ok(BlockNode {
            id: NodeId(format!("odf-{}", self.next_node)),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator,
                confidence: Some(1.0),
            },
        })
    }

    pub(super) fn add_inlines(&mut self, count: usize) -> Result<(), ConversionError> {
        self.inline_count = self
            .inline_count
            .checked_add(count)
            .ok_or_else(|| limit("documentInlines", "ODF inline count overflow"))?;
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Err(limit(
                "documentInlines",
                format!("{} > {MAX_DOCUMENT_INLINES}", self.inline_count),
            ));
        }
        Ok(())
    }

    pub(super) fn warning(
        &mut self,
        code: &str,
        message: impl Into<String>,
        locator: SourceLocator,
    ) {
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            locator: Some(locator),
        });
    }
}

pub(super) fn part_locator(part: &str) -> SourceLocator {
    SourceLocator { part: Some(part.into()), ..SourceLocator::default() }
}

pub(super) fn malformed(part: Option<&str>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: part.map(str::to_owned), detail: detail.into() }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}
