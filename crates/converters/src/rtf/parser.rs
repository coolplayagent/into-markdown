//! Parser orchestration for bounded, non-executing Rich Text Format conversion.

use super::budget::{limit, locator, malformed, reserve_vec};
use super::{PROVIDER_ID, strict_header};
use into_markdown_core::{
    Asset, Block, BlockNode, Cell, ConversionError, ConversionOptions, ConverterOutput, Diagnostic,
    DiagnosticSeverity, Document, ExecutionContext, Inline, MAX_DOCUMENT_NODES, NodeId, Provenance,
    ProvenanceKind, ResourceReservation, SourceLocator, TableRow, estimate_validation_working_set,
};

pub(super) const MAX_CONTROLS: u64 = 1_000_000;
pub(super) const MAX_NUMERIC_DIGITS: usize = 10;
pub(super) const MAX_CONTROL_WORD_LEN: usize = 32;
pub(super) const MAX_DIAGNOSTICS: usize = 4096;
pub(super) const CHECKPOINT_INTERVAL: usize = 4096;
pub(super) const MAX_METADATA_BYTES: usize = 64 * 1024;
pub(super) const MAX_RTF_FONTS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Destination {
    Body,
    FieldContainer,
    InfoContainer,
    ShapePictureContainer,
    Skip,
    FontTable,
    Pict,
    ListText,
    MetaTitle,
    MetaAuthor,
    FieldInstruction,
    FieldResult,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // These independent RTF character flags inherit by group.
pub(super) struct State {
    pub(super) destination: Destination,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) strike: bool,
    pub(super) superscript: bool,
    pub(super) subscript: bool,
    pub(super) ansi_codepage: u16,
    pub(super) font: i32,
    pub(super) unicode_skip: u8,
    pub(super) fallback_remaining: u8,
    pub(super) ignorable: bool,
    pub(super) at_group_start: bool,
    pub(super) in_table: bool,
    pub(super) list_id: Option<i32>,
    pub(super) list_level: Option<u8>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            destination: Destination::Body,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            superscript: false,
            subscript: false,
            ansi_codepage: 1252,
            font: 0,
            unicode_skip: 1,
            fallback_remaining: 0,
            ignorable: false,
            at_group_start: true,
            in_table: false,
            list_id: None,
            list_level: None,
        }
    }
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) state: State,
    pub(super) start: usize,
    pub(super) introduced: Option<Destination>,
}

#[derive(Debug, Default)]
pub(super) struct Paragraph {
    pub(super) inlines: Vec<Inline>,
    pub(super) start: Option<usize>,
    pub(super) end: usize,
}

#[derive(Debug, Default)]
pub(super) struct TableBuilder {
    pub(super) rows: Vec<TableRow>,
    pub(super) cells: Vec<Cell>,
    pub(super) cell_blocks: Vec<BlockNode>,
    pub(super) cell_definitions: Vec<CellMerge>,
    pub(super) cell_definition_index: usize,
    pub(super) pending_cell_merge: CellMerge,
    pub(super) last_cell_boundary: Option<i32>,
    pub(super) row_width: u64,
    pub(super) table_width: Option<u64>,
    pub(super) node_reserved: bool,
    pub(super) active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ListKey {
    pub(super) id: Option<i32>,
    pub(super) level: u8,
    pub(super) kind: into_markdown_core::ListKind,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum CellMerge {
    #[default]
    Normal,
    Start,
    Continue,
}

#[derive(Debug, Default)]
pub(super) struct Picture {
    pub(super) bytes: Vec<u8>,
    pub(super) media_type: Option<&'static str>,
    pub(super) start: usize,
    pub(super) saw_odd_nibble: bool,
    pub(super) high_nibble: Option<u8>,
}

#[derive(Debug, Default)]
pub(super) struct Field {
    pub(super) instruction_seen: bool,
    pub(super) result_seen: bool,
    pub(super) link: Option<String>,
    pub(super) inline_start: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FontCharset {
    pub(super) font: i32,
    pub(super) charset: u16,
    pub(super) order: u32,
}

pub(super) struct Parser<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) options: &'a ConversionOptions,
    pub(super) context: &'a ExecutionContext,
    pub(super) offset: usize,
    pub(super) frames: Vec<Frame>,
    pub(super) fallback_state: State,
    pub(super) root_closed: bool,
    pub(super) paragraph: Paragraph,
    pub(super) table: TableBuilder,
    pub(super) blocks: Vec<BlockNode>,
    pub(super) assets: Vec<Asset>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) metadata: into_markdown_core::DocumentMetadata,
    pub(super) font_charsets: Vec<FontCharset>,
    pub(super) font_table_font: Option<i32>,
    pub(super) capture: String,
    pub(super) picture: Option<Picture>,
    pub(super) pending_list_marker: Option<String>,
    pub(super) last_list_key: Option<ListKey>,
    pub(super) field: Option<Field>,
    pub(super) pending_high_surrogate: Option<(u16, usize, usize)>,
    pub(super) node_sequence: u64,
    pub(super) document_nodes: usize,
    pub(super) control_count: u64,
    pub(super) decoded_bytes: u64,
    pub(super) total_asset_bytes: u64,
    pub(super) table_cells: u64,
    pub(super) memory: ResourceReservation,
}

pub(super) fn parse_rtf(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    Parser::new(bytes, options, context)?.parse()
}

impl<'a> Parser<'a> {
    pub(super) fn new(
        bytes: &'a [u8],
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        context.checkpoint()?;
        let input_len = u64::try_from(bytes.len())
            .map_err(|_| limit("max_input_bytes", "RTF size overflow"))?;
        if input_len > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!("{input_len} > {}", options.limits.max_input_bytes),
            ));
        }
        if options.limits.max_nesting_depth == 0 {
            return Err(limit("max_nesting_depth", "RTF root requires depth 1"));
        }
        if strict_header(bytes).is_none() {
            return Err(malformed("RTF header must begin with {\\rtfN and a delimiter"));
        }
        let mut memory = context.reserve_memory(0)?;
        let mut frames = Vec::new();
        reserve_vec(&mut frames, 1, &mut memory)?;
        frames.push(Frame { state: State::default(), start: 0, introduced: None });
        Ok(Self {
            bytes,
            options,
            context,
            offset: 1,
            frames,
            fallback_state: State::default(),
            root_closed: false,
            paragraph: Paragraph::default(),
            table: TableBuilder::default(),
            blocks: Vec::new(),
            assets: Vec::new(),
            diagnostics: Vec::new(),
            metadata: into_markdown_core::DocumentMetadata::default(),
            font_charsets: Vec::new(),
            font_table_font: None,
            capture: String::new(),
            picture: None,
            pending_list_marker: None,
            last_list_key: None,
            field: None,
            pending_high_surrogate: None,
            node_sequence: 0,
            document_nodes: 0,
            control_count: 0,
            decoded_bytes: 0,
            total_asset_bytes: 0,
            table_cells: 0,
            memory,
        })
    }

    pub(super) fn parse(mut self) -> Result<ConverterOutput, ConversionError> {
        while self.offset < self.bytes.len() {
            if self.root_closed {
                if self.bytes[self.offset..].iter().all(u8::is_ascii_whitespace) {
                    self.offset = self.bytes.len();
                    break;
                }
                return Err(malformed("non-whitespace data follows the root RTF group"));
            }
            if self.offset.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.context.checkpoint()?;
            }
            match self.bytes[self.offset] {
                b'{' => self.open_group()?,
                b'}' => self.close_group()?,
                b'\\' => self.control()?,
                b'\r' | b'\n' => self.offset += 1,
                _ => self.plain_text()?,
            }
        }
        if !self.root_closed || self.frames.len() != 1 {
            return Err(malformed("unterminated RTF group"));
        }
        self.flush_pending_surrogate()?;
        self.finish_table_or_paragraph(self.bytes.len())?;
        let source_is_empty =
            self.blocks.is_empty() && self.assets.is_empty() && self.diagnostics.is_empty();
        if source_is_empty {
            self.add_diagnostic(
                "rtf.emptyDocument",
                DiagnosticSeverity::Info,
                "RTF contains no displayable content",
                Some(locator(0, self.bytes.len())),
            )?;
        }
        let document = Document {
            metadata: std::mem::take(&mut self.metadata),
            blocks: std::mem::take(&mut self.blocks),
            ..Document::default()
        };
        let validation_bytes =
            estimate_validation_working_set(&document, &self.assets, &self.diagnostics)?;
        let validation_memory = self.context.reserve_memory(validation_bytes)?;
        document.validate().map_err(|error| ConversionError::Internal {
            detail: format!("RTF converter produced invalid IR: {error}"),
        })?;
        drop(validation_memory);
        let assets = std::mem::take(&mut self.assets);
        let diagnostics = std::mem::take(&mut self.diagnostics);
        let context = self.context;
        let empty = context.reserve_memory(0)?;
        let memory = std::mem::replace(&mut self.memory, empty);
        // Keep the complete parser peak charged until every transient group, font, capture,
        // table, and decode buffer has been dropped. Only then may the retained-output
        // authority shrink the same authenticated reservation to the live IR/assets.
        drop(self);
        let output = ConverterOutput::new_with_memory_reservation(
            document,
            assets,
            diagnostics,
            context,
            memory,
        )?;
        Ok(if source_is_empty {
            output.with_source_content_evidence(into_markdown_core::SourceContentEvidence::Empty)
        } else {
            output
        })
    }

    pub(super) fn state(&self) -> &State {
        self.frames.last().map_or(&self.fallback_state, |frame| &frame.state)
    }

    pub(super) fn state_mut(&mut self) -> &mut State {
        self.frames.last_mut().map_or(&mut self.fallback_state, |frame| &mut frame.state)
    }

    pub(super) fn node(
        &mut self,
        block: Block,
        start: usize,
        end: usize,
    ) -> Result<BlockNode, ConversionError> {
        self.consume_document_node()?;
        self.node_reserved(block, start, end)
    }

    pub(super) fn node_reserved(
        &mut self,
        block: Block,
        start: usize,
        end: usize,
    ) -> Result<BlockNode, ConversionError> {
        self.node_sequence = self
            .node_sequence
            .checked_add(1)
            .ok_or_else(|| limit("document_nodes", "node ID overflow"))?;
        // Node IDs are a fixed prefix plus u64 decimal; provider is fixed. Prepay their maximum.
        self.memory.grow(96)?;
        let id = format!("rtf-{}", self.node_sequence);
        Ok(BlockNode {
            id: NodeId(id),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: locator(start, end),
                confidence: Some(1.0),
            },
        })
    }

    pub(super) fn push_block(&mut self, block: BlockNode) -> Result<(), ConversionError> {
        reserve_vec(&mut self.blocks, 1, &mut self.memory)?;
        self.blocks.push(block);
        self.last_list_key = None;
        Ok(())
    }

    pub(super) fn consume_document_node(&mut self) -> Result<(), ConversionError> {
        self.ensure_document_nodes(1)?;
        self.document_nodes = self.document_nodes.saturating_add(1);
        Ok(())
    }

    pub(super) fn ensure_document_nodes(&self, additional: usize) -> Result<(), ConversionError> {
        let needed = self
            .document_nodes
            .checked_add(additional)
            .ok_or_else(|| limit("document_nodes", "structural node count overflow"))?;
        if needed > MAX_DOCUMENT_NODES {
            return Err(limit("document_nodes", format!("{needed} > {MAX_DOCUMENT_NODES}")));
        }
        Ok(())
    }

    pub(super) fn add_diagnostic(
        &mut self,
        code: &str,
        severity: DiagnosticSeverity,
        message: &str,
        locator: Option<SourceLocator>,
    ) -> Result<(), ConversionError> {
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            return Err(limit("rtf_diagnostics", format!(">= {MAX_DIAGNOSTICS}")));
        }
        self.memory
            .grow(u64::try_from(code.len().saturating_add(message.len())).unwrap_or(u64::MAX))?;
        reserve_vec(&mut self.diagnostics, 1, &mut self.memory)?;
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity,
            message: message.into(),
            locator,
        });
        Ok(())
    }
}
