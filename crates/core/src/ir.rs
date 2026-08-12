use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable node identifier within a document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Stable embedded-asset identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub String);

/// Rectangle in source coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// Spreadsheet cell address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRef {
    /// Zero-based row.
    pub row: u32,
    /// Zero-based column.
    pub column: u32,
}

/// Time range in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    /// Inclusive start.
    pub start_ms: u64,
    /// Exclusive end.
    pub end_ms: u64,
}

/// Location of extracted content in the source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceLocator {
    /// One-based page number.
    pub page: Option<u32>,
    /// One-based slide number.
    pub slide: Option<u32>,
    /// Worksheet name.
    pub sheet: Option<String>,
    /// Spreadsheet cell.
    pub cell: Option<CellRef>,
    /// Bounding rectangle.
    pub bounds: Option<Rect>,
    /// Media time range.
    pub time: Option<TimeRange>,
    /// Format-specific part name that is safe to expose.
    pub part: Option<String>,
}

/// How content entered the IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProvenanceKind {
    /// Deterministic source parser.
    NativeParser,
    /// Local OCR model.
    LocalOcr,
    /// Remote or local AI provider.
    AiProvider,
    /// Container or file metadata.
    Metadata,
    /// Deterministic post-processing.
    Postprocessor,
}

/// Provenance attached to every material IR node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Origin class.
    pub kind: ProvenanceKind,
    /// Stable implementation or provider ID.
    pub provider: String,
    /// Location in the source.
    pub locator: SourceLocator,
    /// Confidence in the inclusive range `0.0..=1.0`, when meaningful.
    pub confidence: Option<f32>,
}

/// Inline formatting mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InlineMark {
    /// Bold text.
    Bold,
    /// Italic text.
    Italic,
    /// Struck text.
    Strikethrough,
    /// Underlined source text.
    Underline,
    /// Superscript source text.
    Superscript,
    /// Subscript source text.
    Subscript,
}

/// Inline content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Inline {
    /// Plain or styled text.
    Text {
        /// Source-derived text.
        value: String,
        /// Active formatting marks.
        marks: Vec<InlineMark>,
    },
    /// Inline code.
    Code(String),
    /// Link with structured label content.
    Link {
        /// Link destination.
        target: String,
        /// Linked label content.
        content: Vec<Self>,
    },
    /// Inline formula, preferably LaTeX.
    Formula(String),
    /// Explicit source line break.
    LineBreak,
}

/// List marker family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListKind {
    /// Bulleted list.
    Bullet,
    /// Ordered list.
    Ordered,
    /// Task list.
    Task,
}

/// List item containing arbitrary blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    /// Optional task state.
    pub checked: Option<bool>,
    /// Source marker when Markdown cannot represent it exactly.
    pub marker_label: Option<String>,
    /// Item contents.
    pub blocks: Vec<BlockNode>,
}

/// Table cell containing arbitrary blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// Row span, at least one after validation.
    pub row_span: u32,
    /// Column span, at least one after validation.
    pub column_span: u32,
    /// Whether this is a header cell.
    pub header: bool,
    /// Cell contents.
    pub blocks: Vec<BlockNode>,
}

/// Logical table row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableRow {
    /// Origin cells in display order.
    pub cells: Vec<Cell>,
}

/// Structural block content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Block {
    /// Paragraph.
    Paragraph(Vec<Inline>),
    /// Heading.
    Heading {
        /// Source heading level, normalized during validation.
        level: u8,
        /// Heading contents.
        content: Vec<Inline>,
    },
    /// Nested list.
    List {
        /// Marker family.
        kind: ListKind,
        /// First ordered-list value.
        start: u64,
        /// Ordered list items.
        items: Vec<ListItem>,
    },
    /// Table.
    Table {
        /// Logical rows in reading order.
        rows: Vec<TableRow>,
    },
    /// Fenced code block.
    Code {
        /// Optional source language identifier.
        language: Option<String>,
        /// Literal block contents.
        text: String,
    },
    /// Display formula, preferably LaTeX.
    Formula(String),
    /// Footnote definition.
    Footnote {
        /// Source label or stable generated label.
        label: String,
        /// Footnote contents.
        blocks: Vec<BlockNode>,
    },
    /// Image or visual asset.
    Image {
        /// Referenced asset.
        asset: AssetId,
        /// Source, OCR, or provider-derived alternative text.
        alt: Option<String>,
    },
    /// Page boundary with nested content.
    Page {
        /// One-based page number.
        number: u32,
        /// Page contents.
        blocks: Vec<BlockNode>,
    },
    /// Presentation slide.
    Slide {
        /// One-based slide number.
        number: u32,
        /// Optional resolved slide title.
        title: Option<String>,
        /// Slide body and speaker notes.
        blocks: Vec<BlockNode>,
    },
    /// Worksheet.
    Sheet {
        /// Worksheet name.
        name: String,
        /// Sheet tables, drawings, and annotations.
        blocks: Vec<BlockNode>,
    },
    /// Time-aligned audio/video text.
    TimedSegment {
        /// Media time range.
        range: TimeRange,
        /// Optional resolved speaker label.
        speaker: Option<String>,
        /// Segment transcript.
        content: Vec<Inline>,
    },
    /// Horizontal rule.
    Rule,
}

/// Block plus identity and provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockNode {
    /// Document-scoped stable identifier.
    pub id: NodeId,
    /// Block contents.
    pub block: Block,
    /// Extraction provenance.
    pub provenance: Provenance,
}

/// Embedded or external resource returned separately from Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// Stable document-scoped ID.
    pub id: AssetId,
    /// Suggested filename.
    pub filename: Option<String>,
    /// MIME media type.
    pub media_type: String,
    /// Raw asset bytes. External assets may leave this empty.
    pub bytes: Vec<u8>,
    /// Original external URI when the source contains one.
    pub external_uri: Option<String>,
}

/// General source metadata that remains deterministic and non-secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    /// Document title.
    pub title: Option<String>,
    /// Document authors.
    pub authors: Vec<String>,
    /// Additional namespaced string properties.
    pub properties: BTreeMap<String, String>,
}

/// Format-independent document representation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Document metadata.
    pub metadata: DocumentMetadata,
    /// Body content in source reading order.
    pub blocks: Vec<BlockNode>,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Informational recovery note.
    Info,
    /// Content was skipped or recovered imperfectly.
    Warning,
    /// A scoped operation failed but conversion continued.
    Error,
}

/// Structured non-fatal diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable machine-readable code.
    pub code: String,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Human-readable message.
    pub message: String,
    /// Optional source location.
    pub locator: Option<SourceLocator>,
}
