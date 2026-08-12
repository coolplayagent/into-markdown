use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use thiserror::Error;

/// JSON schema version emitted and accepted by this library.
pub const DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// Maximum nested block-node depth accepted by the default validator.
pub const MAX_DOCUMENT_DEPTH: usize = 16;
/// Maximum structural-node count accepted by the default validator.
pub const MAX_DOCUMENT_NODES: usize = 100_000;
/// Maximum inline-node count accepted by the default validator.
pub const MAX_DOCUMENT_INLINES: usize = 1_000_000;
/// Maximum UTF-8 JSON input size accepted by the default decoder.
pub const MAX_DOCUMENT_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Maximum logical column count accepted for one table.
pub const MAX_TABLE_COLUMNS: usize = 16_384;

/// Structural budgets applied while validating untrusted document IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationLimits {
    /// Maximum nested block-node depth; document-root nodes have depth one.
    pub max_depth: usize,
    /// Maximum number of block, list-item, table-row, and table-cell nodes.
    pub max_nodes: usize,
    /// Maximum number of inline nodes across the document.
    pub max_inlines: usize,
    /// Maximum UTF-8 byte length accepted by the JSON decoder.
    pub max_json_bytes: usize,
    /// Maximum logical width of any table.
    pub max_table_columns: usize,
}

impl Default for ValidationLimits {
    fn default() -> Self {
        Self {
            max_depth: MAX_DOCUMENT_DEPTH,
            max_nodes: MAX_DOCUMENT_NODES,
            max_inlines: MAX_DOCUMENT_INLINES,
            max_json_bytes: MAX_DOCUMENT_JSON_BYTES,
            max_table_columns: MAX_TABLE_COLUMNS,
        }
    }
}

/// Stable categories returned while decoding or validating document IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum IrErrorCode {
    /// The input is not syntactically valid JSON or does not match the schema.
    InvalidJson,
    /// The document declares a schema version this library cannot read.
    UnsupportedSchemaVersion,
    /// A node violates a structural or semantic invariant.
    InvalidNode,
    /// A provenance record is missing or internally inconsistent.
    InvalidProvenance,
    /// A source locator is invalid.
    InvalidLocator,
    /// A document-scoped node identifier occurs more than once.
    DuplicateNodeId,
    /// A structural validation budget was exceeded.
    ResourceLimit,
}

impl IrErrorCode {
    /// Stable lower-camel-case representation for machine consumers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalidJson",
            Self::UnsupportedSchemaVersion => "unsupportedSchemaVersion",
            Self::InvalidNode => "invalidNode",
            Self::InvalidProvenance => "invalidProvenance",
            Self::InvalidLocator => "invalidLocator",
            Self::DuplicateNodeId => "duplicateNodeId",
            Self::ResourceLimit => "resourceLimit",
        }
    }
}

/// Controlled failure from an IR wire or validation boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {path}: {detail}", code = code.as_str())]
pub struct IrError {
    /// Stable machine-readable category.
    pub code: IrErrorCode,
    /// Stable JSON-style path to the rejected value.
    pub path: String,
    /// Human-readable explanation; callers must branch on [`Self::code`].
    pub detail: String,
}

impl IrError {
    fn new(code: IrErrorCode, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { code, path: path.into(), detail: detail.into() }
    }
}

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
#[serde(rename_all = "camelCase")]
pub struct CellRef {
    /// Zero-based row.
    pub row: u32,
    /// Zero-based column.
    pub column: u32,
}

/// Time range in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeRange {
    /// Inclusive start.
    pub start_ms: u64,
    /// Exclusive end.
    pub end_ms: u64,
}

/// Location of extracted content in the source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    /// Safe container-relative part name, using `/` separators.
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
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
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
    /// Reference to a document footnote definition.
    FootnoteReference(String),
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct TableRow {
    /// Origin cells in display order.
    pub cells: Vec<Cell>,
}

/// Structural block content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct DocumentMetadata {
    /// Document title.
    pub title: Option<String>,
    /// Document authors.
    pub authors: Vec<String>,
    /// Additional namespaced string properties.
    pub properties: BTreeMap<String, String>,
}

/// Format-independent document representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    /// Version of the serialized document contract.
    pub schema_version: u32,
    /// Document metadata.
    pub metadata: DocumentMetadata,
    /// Body content in source reading order.
    pub blocks: Vec<BlockNode>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            metadata: DocumentMetadata::default(),
            blocks: Vec::new(),
        }
    }
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
#[serde(rename_all = "camelCase")]
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

impl Document {
    /// Serialize a validated document using the stable JSON contract.
    ///
    /// # Errors
    ///
    /// Returns a stable [`IrErrorCode`] when the document is invalid or JSON
    /// serialization fails.
    pub fn to_json(&self) -> Result<String, IrError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|error| {
            IrError::new(IrErrorCode::InvalidJson, "$", format!("serialize document: {error}"))
        })
    }

    /// Decode and validate a document from the stable JSON contract.
    ///
    /// Unknown object fields are ignored for additive compatibility within a
    /// schema version. A different schema version is rejected before use.
    ///
    /// # Errors
    ///
    /// Returns [`IrErrorCode::InvalidJson`] for malformed JSON/schema shapes,
    /// or the applicable validation code for invalid IR.
    pub fn from_json(json: &str) -> Result<Self, IrError> {
        Self::from_json_with_limits(json, &ValidationLimits::default())
    }

    /// Decode and validate a document with explicit structural budgets.
    ///
    /// # Errors
    ///
    /// Returns the same stable errors as [`Self::from_json`], including
    /// [`IrErrorCode::ResourceLimit`] when a supplied budget is exceeded.
    pub fn from_json_with_limits(json: &str, limits: &ValidationLimits) -> Result<Self, IrError> {
        if json.len() > limits.max_json_bytes {
            return resource_limit("$", "documentJsonBytes", limits.max_json_bytes);
        }
        let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
            IrError::new(IrErrorCode::InvalidJson, "$", format!("decode document: {error}"))
        })?;
        let version =
            value.get("schemaVersion").and_then(serde_json::Value::as_u64).ok_or_else(|| {
                IrError::new(
                    IrErrorCode::InvalidJson,
                    "$.schemaVersion",
                    "schemaVersion must be an unsigned integer",
                )
            })?;
        if version != u64::from(DOCUMENT_SCHEMA_VERSION) {
            return Err(IrError::new(
                IrErrorCode::UnsupportedSchemaVersion,
                "$.schemaVersion",
                format!("expected {DOCUMENT_SCHEMA_VERSION}, got {version}"),
            ));
        }
        preflight_document_value(&value, limits)?;
        let document: Self = serde_json::from_value(value).map_err(|error| {
            IrError::new(IrErrorCode::InvalidJson, "$", format!("decode document: {error}"))
        })?;
        document.validate_with_limits(limits)?;
        Ok(document)
    }

    /// Validate the schema version and all recursive IR invariants.
    ///
    /// # Errors
    ///
    /// Returns a stable, path-addressed [`IrError`] without panicking.
    pub fn validate(&self) -> Result<(), IrError> {
        self.validate_with_limits(&ValidationLimits::default())
    }

    /// Validate all IR invariants with explicit structural budgets.
    ///
    /// # Errors
    ///
    /// Returns a stable, path-addressed [`IrError`], including
    /// [`IrErrorCode::ResourceLimit`] when a budget is exceeded.
    pub fn validate_with_limits(&self, limits: &ValidationLimits) -> Result<(), IrError> {
        if self.schema_version != DOCUMENT_SCHEMA_VERSION {
            return Err(IrError::new(
                IrErrorCode::UnsupportedSchemaVersion,
                "$.schemaVersion",
                format!("expected {DOCUMENT_SCHEMA_VERSION}, got {}", self.schema_version),
            ));
        }
        let mut state = ValidationState::new(limits);
        validate_nodes(&self.blocks, "$.blocks", 1, &mut state)?;
        if let Some(label) = state.footnote_references.difference(&state.footnotes).next() {
            return invalid_node("$.blocks", format!("undefined footnote reference {label}"));
        }
        Ok(())
    }
}

struct ValidationState<'a> {
    limits: &'a ValidationLimits,
    node_count: usize,
    inline_count: usize,
    node_ids: BTreeSet<String>,
    footnotes: BTreeSet<String>,
    footnote_references: BTreeSet<String>,
}

impl<'a> ValidationState<'a> {
    fn new(limits: &'a ValidationLimits) -> Self {
        Self {
            limits,
            node_count: 0,
            inline_count: 0,
            node_ids: BTreeSet::new(),
            footnotes: BTreeSet::new(),
            footnote_references: BTreeSet::new(),
        }
    }
}

enum PreflightTask<'a> {
    Block { value: &'a serde_json::Value, path: String, depth: usize },
    Inline { value: &'a serde_json::Value, path: String },
    ListItem { value: &'a serde_json::Value, path: String, depth: usize },
    TableRow { value: &'a serde_json::Value, path: String, depth: usize },
    TableCell { value: &'a serde_json::Value, path: String, depth: usize },
}

#[allow(clippy::too_many_lines)] // Keeping the non-recursive wire state machine together is safer.
fn preflight_document_value(
    document: &serde_json::Value,
    limits: &ValidationLimits,
) -> Result<(), IrError> {
    let Some(blocks) = document.get("blocks").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    let mut stack = Vec::new();
    push_block_tasks(&mut stack, blocks, "$.blocks", 1);
    let mut structural_nodes = 0_usize;
    let mut inline_nodes = 0_usize;

    while let Some(task) = stack.pop() {
        match task {
            PreflightTask::Block { value, path, depth } => {
                let Some(object) = value.as_object() else { continue };
                let Some(block) = object.get("block").and_then(serde_json::Value::as_object) else {
                    continue;
                };
                if object.get("id").and_then(serde_json::Value::as_str).is_none()
                    || !object.get("provenance").is_some_and(serde_json::Value::is_object)
                {
                    continue;
                }
                let Some(kind) = block.get("type").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let data = block.get("data");
                if !valid_block_wire_shape(kind, data) {
                    continue;
                }
                if depth > limits.max_depth {
                    return resource_limit(&path, "documentDepth", limits.max_depth);
                }
                consume_preflight_node(&mut structural_nodes, limits, &path)?;
                let data_path = format!("{path}.block.data");
                match kind {
                    "paragraph" => {
                        if let Some(inlines) = data.and_then(serde_json::Value::as_array) {
                            push_inline_tasks(&mut stack, inlines, &data_path);
                        }
                    }
                    "heading" | "timedSegment" => push_inline_property(
                        &mut stack,
                        data,
                        "content",
                        &format!("{data_path}.content"),
                    ),
                    "list" => push_list_item_tasks(
                        &mut stack,
                        data,
                        &format!("{data_path}.items"),
                        depth + 1,
                    ),
                    "table" => {
                        preflight_table_width(data, &data_path, limits)?;
                        push_table_row_tasks(
                            &mut stack,
                            data,
                            &format!("{data_path}.rows"),
                            depth + 1,
                        );
                    }
                    "footnote" | "page" | "slide" | "sheet" => push_block_property(
                        &mut stack,
                        data,
                        "blocks",
                        &format!("{data_path}.blocks"),
                        depth + 1,
                    ),
                    _ => {}
                }
            }
            PreflightTask::Inline { value, path } => {
                let Some(object) = value.as_object() else { continue };
                let Some(kind) = object.get("type").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if !valid_inline_wire_shape(kind, object.get("data")) {
                    continue;
                }
                inline_nodes = inline_nodes.saturating_add(1);
                if inline_nodes > limits.max_inlines {
                    return resource_limit(&path, "documentInlines", limits.max_inlines);
                }
                if kind == "link" {
                    push_inline_property(
                        &mut stack,
                        object.get("data"),
                        "content",
                        &format!("{path}.data.content"),
                    );
                }
            }
            PreflightTask::ListItem { value, path, depth } => {
                let Some(object) = value.as_object() else { continue };
                let Some(blocks) = object.get("blocks").and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                consume_preflight_node(&mut structural_nodes, limits, &path)?;
                push_block_tasks(&mut stack, blocks, &format!("{path}.blocks"), depth);
            }
            PreflightTask::TableRow { value, path, depth } => {
                let Some(cells) = value.get("cells").and_then(serde_json::Value::as_array) else {
                    continue;
                };
                consume_preflight_node(&mut structural_nodes, limits, &path)?;
                for (index, cell) in cells.iter().enumerate().rev() {
                    stack.push(PreflightTask::TableCell {
                        value: cell,
                        path: format!("{path}.cells[{index}]"),
                        depth,
                    });
                }
            }
            PreflightTask::TableCell { value, path, depth } => {
                let Some(object) = value.as_object() else { continue };
                let Some(blocks) = object.get("blocks").and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                if object.get("rowSpan").and_then(serde_json::Value::as_u64).is_none()
                    || object.get("columnSpan").and_then(serde_json::Value::as_u64).is_none()
                {
                    continue;
                }
                consume_preflight_node(&mut structural_nodes, limits, &path)?;
                push_block_tasks(&mut stack, blocks, &format!("{path}.blocks"), depth);
            }
        }
    }
    Ok(())
}

fn valid_block_wire_shape(kind: &str, data: Option<&serde_json::Value>) -> bool {
    match kind {
        "paragraph" => data.is_some_and(serde_json::Value::is_array),
        "formula" => data.is_some_and(serde_json::Value::is_string),
        "heading" | "list" | "table" | "code" | "footnote" | "image" | "page" | "slide"
        | "sheet" | "timedSegment" => data.is_some_and(serde_json::Value::is_object),
        "rule" => true,
        _ => false,
    }
}

fn valid_inline_wire_shape(kind: &str, data: Option<&serde_json::Value>) -> bool {
    match kind {
        "text" | "link" => data.is_some_and(serde_json::Value::is_object),
        "code" | "formula" | "footnoteReference" => data.is_some_and(serde_json::Value::is_string),
        "lineBreak" => true,
        _ => false,
    }
}

fn push_block_tasks<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    blocks: &'a [serde_json::Value],
    path: &str,
    depth: usize,
) {
    for (index, block) in blocks.iter().enumerate().rev() {
        stack.push(PreflightTask::Block { value: block, path: format!("{path}[{index}]"), depth });
    }
}

fn push_inline_tasks<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    inlines: &'a [serde_json::Value],
    path: &str,
) {
    for (index, inline) in inlines.iter().enumerate().rev() {
        stack.push(PreflightTask::Inline { value: inline, path: format!("{path}[{index}]") });
    }
}

fn push_block_property<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    data: Option<&'a serde_json::Value>,
    property: &str,
    path: &str,
    depth: usize,
) {
    if let Some(blocks) = data
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get(property))
        .and_then(serde_json::Value::as_array)
    {
        push_block_tasks(stack, blocks, path, depth);
    }
}

fn push_inline_property<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    data: Option<&'a serde_json::Value>,
    property: &str,
    path: &str,
) {
    if let Some(inlines) = data
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get(property))
        .and_then(serde_json::Value::as_array)
    {
        push_inline_tasks(stack, inlines, path);
    }
}

fn push_list_item_tasks<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    data: Option<&'a serde_json::Value>,
    path: &str,
    depth: usize,
) {
    if let Some(items) = data
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get("items"))
        .and_then(serde_json::Value::as_array)
    {
        for (index, item) in items.iter().enumerate().rev() {
            stack.push(PreflightTask::ListItem {
                value: item,
                path: format!("{path}[{index}]"),
                depth,
            });
        }
    }
}

fn push_table_row_tasks<'a>(
    stack: &mut Vec<PreflightTask<'a>>,
    data: Option<&'a serde_json::Value>,
    path: &str,
    depth: usize,
) {
    if let Some(rows) = data
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get("rows"))
        .and_then(serde_json::Value::as_array)
    {
        for (index, row) in rows.iter().enumerate().rev() {
            stack.push(PreflightTask::TableRow {
                value: row,
                path: format!("{path}[{index}]"),
                depth,
            });
        }
    }
}

fn consume_preflight_node(
    count: &mut usize,
    limits: &ValidationLimits,
    path: &str,
) -> Result<(), IrError> {
    *count = count.saturating_add(1);
    if *count > limits.max_nodes {
        resource_limit(path, "documentNodes", limits.max_nodes)
    } else {
        Ok(())
    }
}

fn preflight_table_width(
    data: Option<&serde_json::Value>,
    path: &str,
    limits: &ValidationLimits,
) -> Result<(), IrError> {
    let Some(rows) = data
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get("rows"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    let mut occupancy = Vec::<u64>::new();
    for (row_index, row) in rows.iter().enumerate() {
        let Some(cells) = row.get("cells").and_then(serde_json::Value::as_array) else {
            return Ok(());
        };
        let mut column = 0_usize;
        for (cell_index, cell) in cells.iter().enumerate() {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let Some(row_span) = cell.get("rowSpan").and_then(serde_json::Value::as_u64) else {
                return Ok(());
            };
            let Some(column_span) = cell
                .get("columnSpan")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
            else {
                return Ok(());
            };
            if row_span == 0 || column_span == 0 {
                return Ok(());
            }
            let cell_path = format!("{path}.rows[{row_index}].cells[{cell_index}]");
            let Some(end) = column.checked_add(column_span) else {
                return resource_limit(
                    format!("{cell_path}.columnSpan"),
                    "tableColumns",
                    limits.max_table_columns,
                );
            };
            if end > limits.max_table_columns {
                return resource_limit(
                    format!("{cell_path}.columnSpan"),
                    "tableColumns",
                    limits.max_table_columns,
                );
            }
            if occupancy.len() < end {
                occupancy.resize(end, 0);
            }
            occupancy[column..end].fill(row_span);
            column = end;
        }
        for remaining in &mut occupancy {
            *remaining = remaining.saturating_sub(1);
        }
    }
    Ok(())
}

fn validate_nodes(
    nodes: &[BlockNode],
    path: &str,
    depth: usize,
    state: &mut ValidationState<'_>,
) -> Result<(), IrError> {
    if !nodes.is_empty() && depth > state.limits.max_depth {
        return resource_limit(path, "documentDepth", state.limits.max_depth);
    }
    for (index, node) in nodes.iter().enumerate() {
        let node_path = format!("{path}[{index}]");
        consume_structural_node(state, &node_path)?;
        if node.id.0.trim().is_empty() {
            return Err(IrError::new(
                IrErrorCode::InvalidNode,
                format!("{node_path}.id"),
                "node ID must not be empty",
            ));
        }
        if !state.node_ids.insert(node.id.0.clone()) {
            return Err(IrError::new(
                IrErrorCode::DuplicateNodeId,
                format!("{node_path}.id"),
                format!("duplicate node ID {}", node.id.0),
            ));
        }
        validate_provenance(&node.provenance, &format!("{node_path}.provenance"))?;
        validate_block(&node.block, &format!("{node_path}.block"), depth, state)?;
    }
    Ok(())
}

fn validate_provenance(provenance: &Provenance, path: &str) -> Result<(), IrError> {
    if provenance.provider.trim().is_empty() {
        return Err(IrError::new(
            IrErrorCode::InvalidProvenance,
            format!("{path}.provider"),
            "provider ID must not be empty",
        ));
    }
    if provenance
        .confidence
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(IrError::new(
            IrErrorCode::InvalidProvenance,
            format!("{path}.confidence"),
            "confidence must be finite and in 0.0..=1.0",
        ));
    }
    validate_locator(&provenance.locator, &format!("{path}.locator"))
}

fn validate_locator(locator: &SourceLocator, path: &str) -> Result<(), IrError> {
    if locator.page == Some(0) {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.page"),
            "page numbers are one-based",
        ));
    }
    if locator.slide == Some(0) {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.slide"),
            "slide numbers are one-based",
        ));
    }
    if locator.sheet.as_ref().is_some_and(|value| value.trim().is_empty()) {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.sheet"),
            "worksheet name must not be empty",
        ));
    }
    if locator.cell.is_some() && locator.sheet.is_none() {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.cell"),
            "cell coordinates require a worksheet name",
        ));
    }
    if let Some(bounds) = locator.bounds
        && (!bounds.x.is_finite()
            || !bounds.y.is_finite()
            || !bounds.width.is_finite()
            || !bounds.height.is_finite()
            || bounds.width < 0.0
            || bounds.height < 0.0)
    {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.bounds"),
            "bounds must be finite with non-negative dimensions",
        ));
    }
    if let Some(range) = locator.time
        && range.start_ms >= range.end_ms
    {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            format!("{path}.time"),
            "time range start must precede end",
        ));
    }
    if let Some(part) = &locator.part {
        validate_part_name(part, &format!("{path}.part"))?;
    }
    Ok(())
}

fn validate_part_name(part: &str, path: &str) -> Result<(), IrError> {
    let has_drive_prefix = part.as_bytes().get(1) == Some(&b':')
        && part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    let invalid = part.is_empty()
        || part.starts_with('/')
        || has_drive_prefix
        || part.contains('\\')
        || part.chars().any(char::is_control)
        || part.split('/').any(|segment| segment.is_empty() || matches!(segment, "." | ".."));
    if invalid {
        return Err(IrError::new(
            IrErrorCode::InvalidLocator,
            path,
            "part must be a safe container-relative path",
        ));
    }
    Ok(())
}

fn validate_block(
    block: &Block,
    path: &str,
    depth: usize,
    state: &mut ValidationState<'_>,
) -> Result<(), IrError> {
    match block {
        Block::Paragraph(content) => {
            validate_inlines(content, &format!("{path}.data"), false, state)
        }
        Block::Heading { level, content } => {
            if !(1..=6).contains(level) {
                return invalid_node(format!("{path}.data.level"), "heading level must be 1..=6");
            }
            validate_inlines(content, &format!("{path}.data.content"), false, state)
        }
        Block::List { kind, start, items } => {
            validate_list(*kind, *start, items, path, depth, state)
        }
        Block::Table { rows } => validate_table(rows, path, depth, state),
        Block::Code { language, .. } => {
            if language.as_ref().is_some_and(|value| value.trim().is_empty()) {
                return invalid_node(
                    format!("{path}.data.language"),
                    "code language must not be empty",
                );
            }
            Ok(())
        }
        Block::Formula(value) => nonempty(value, &format!("{path}.data"), "formula"),
        Block::Footnote { label, blocks } => {
            nonempty(label, &format!("{path}.data.label"), "footnote label")?;
            if !state.footnotes.insert(label.clone()) {
                return invalid_node(
                    format!("{path}.data.label"),
                    format!("duplicate footnote label {label}"),
                );
            }
            validate_nodes(blocks, &format!("{path}.data.blocks"), depth + 1, state)
        }
        Block::Image { asset, .. } => nonempty(&asset.0, &format!("{path}.data.asset"), "asset ID"),
        Block::Page { number, blocks } => {
            positive(*number, &format!("{path}.data.number"), "page number")?;
            validate_nodes(blocks, &format!("{path}.data.blocks"), depth + 1, state)
        }
        Block::Slide { number, blocks, .. } => {
            positive(*number, &format!("{path}.data.number"), "slide number")?;
            validate_nodes(blocks, &format!("{path}.data.blocks"), depth + 1, state)
        }
        Block::Sheet { name, blocks } => {
            nonempty(name, &format!("{path}.data.name"), "worksheet name")?;
            validate_nodes(blocks, &format!("{path}.data.blocks"), depth + 1, state)
        }
        Block::TimedSegment { range, content, .. } => {
            if range.start_ms >= range.end_ms {
                return invalid_node(
                    format!("{path}.data.range"),
                    "time range start must precede end",
                );
            }
            validate_inlines(content, &format!("{path}.data.content"), false, state)
        }
        Block::Rule => Ok(()),
    }
}

fn validate_list(
    kind: ListKind,
    start: u64,
    items: &[ListItem],
    path: &str,
    depth: usize,
    state: &mut ValidationState<'_>,
) -> Result<(), IrError> {
    if items.is_empty() {
        return invalid_node(format!("{path}.data.items"), "list must contain an item");
    }
    if kind == ListKind::Ordered && start == 0 {
        return invalid_node(format!("{path}.data.start"), "ordered list start must be positive");
    }
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}.data.items[{index}]");
        consume_structural_node(state, &item_path)?;
        if (kind == ListKind::Task) != item.checked.is_some() {
            return invalid_node(
                format!("{item_path}.checked"),
                "checked must be present only for task-list items",
            );
        }
        validate_nodes(&item.blocks, &format!("{item_path}.blocks"), depth + 1, state)?;
    }
    Ok(())
}

fn validate_table(
    rows: &[TableRow],
    path: &str,
    depth: usize,
    state: &mut ValidationState<'_>,
) -> Result<(), IrError> {
    if rows.is_empty() {
        return invalid_node(format!("{path}.data.rows"), "table must contain a row");
    }
    let mut occupancy = Vec::<u32>::new();
    let mut table_width = None;
    for (row_index, row) in rows.iter().enumerate() {
        let row_path = format!("{path}.data.rows[{row_index}]");
        consume_structural_node(state, &row_path)?;
        let mut column = 0_usize;
        for (cell_index, cell) in row.cells.iter().enumerate() {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let cell_path = format!("{row_path}.cells[{cell_index}]");
            consume_structural_node(state, &cell_path)?;
            if cell.row_span == 0 || cell.column_span == 0 {
                return invalid_node(
                    format!("{cell_path}.rowSpan"),
                    "table spans must be positive",
                );
            }
            let column_span = usize::try_from(cell.column_span).map_err(|_| {
                IrError::new(
                    IrErrorCode::ResourceLimit,
                    format!("{cell_path}.columnSpan"),
                    "column span cannot be represented on this platform",
                )
            })?;
            let end = column.checked_add(column_span).ok_or_else(|| {
                IrError::new(
                    IrErrorCode::ResourceLimit,
                    format!("{cell_path}.columnSpan"),
                    "logical table width overflowed",
                )
            })?;
            if end > state.limits.max_table_columns {
                return resource_limit(
                    format!("{cell_path}.columnSpan"),
                    "tableColumns",
                    state.limits.max_table_columns,
                );
            }
            if table_width.is_some_and(|width| end > width) {
                return invalid_node(
                    format!("{cell_path}.columnSpan"),
                    "cell extends beyond the table's logical width",
                );
            }
            if occupancy.get(column..end).is_some_and(|slots| slots.iter().any(|value| *value > 0))
            {
                return invalid_node(
                    format!("{cell_path}.columnSpan"),
                    "cell overlaps a row-spanning cell",
                );
            }
            if occupancy.len() < end {
                occupancy.resize(end, 0);
            }
            occupancy[column..end].fill(cell.row_span);
            column = end;
            validate_nodes(&cell.blocks, &format!("{cell_path}.blocks"), depth + 1, state)?;
        }
        let width = *table_width.get_or_insert(occupancy.len());
        if width == 0 {
            return invalid_node(format!("{row_path}.cells"), "table must have a logical column");
        }
        if occupancy.len() != width || occupancy.contains(&0) {
            return invalid_node(row_path, "table rows must have the same logical width");
        }
        for remaining in &mut occupancy {
            *remaining -= 1;
        }
    }
    if occupancy.iter().any(|remaining| *remaining > 0) {
        return invalid_node(
            format!("{path}.data.rows"),
            "row span extends beyond the final table row",
        );
    }
    Ok(())
}

fn validate_inlines(
    content: &[Inline],
    path: &str,
    inside_link: bool,
    state: &mut ValidationState<'_>,
) -> Result<(), IrError> {
    for (index, inline) in content.iter().enumerate() {
        let inline_path = format!("{path}[{index}]");
        state.inline_count = state.inline_count.saturating_add(1);
        if state.inline_count > state.limits.max_inlines {
            return resource_limit(&inline_path, "documentInlines", state.limits.max_inlines);
        }
        match inline {
            Inline::Text { marks, .. } => {
                let unique = marks.iter().copied().collect::<BTreeSet<_>>();
                if unique.len() != marks.len() {
                    return invalid_node(
                        format!("{inline_path}.data.marks"),
                        "text marks must be unique",
                    );
                }
                if unique.contains(&InlineMark::Superscript)
                    && unique.contains(&InlineMark::Subscript)
                {
                    return invalid_node(
                        format!("{inline_path}.data.marks"),
                        "text cannot be both superscript and subscript",
                    );
                }
            }
            Inline::Code(_) | Inline::LineBreak => {}
            Inline::Link { target, content } => {
                nonempty(target, &format!("{inline_path}.data.target"), "link target")?;
                if inside_link {
                    return invalid_node(inline_path, "links must not be nested");
                }
                validate_inlines(content, &format!("{inline_path}.data.content"), true, state)?;
            }
            Inline::Formula(value) => nonempty(value, &format!("{inline_path}.data"), "formula")?,
            Inline::FootnoteReference(label) => {
                nonempty(label, &format!("{inline_path}.data"), "footnote reference")?;
                state.footnote_references.insert(label.clone());
            }
        }
    }
    Ok(())
}

fn positive(value: u32, path: &str, label: &str) -> Result<(), IrError> {
    if value == 0 { invalid_node(path, format!("{label} must be positive")) } else { Ok(()) }
}

fn nonempty(value: &str, path: &str, label: &str) -> Result<(), IrError> {
    if value.trim().is_empty() {
        invalid_node(path, format!("{label} must not be empty"))
    } else {
        Ok(())
    }
}

fn invalid_node<T>(path: impl Into<String>, detail: impl Into<String>) -> Result<T, IrError> {
    Err(IrError::new(IrErrorCode::InvalidNode, path, detail))
}

fn resource_limit<T>(path: impl Into<String>, limit: &str, maximum: usize) -> Result<T, IrError> {
    Err(IrError::new(
        IrErrorCode::ResourceLimit,
        path,
        format!("{limit} limit exceeded (maximum {maximum})"),
    ))
}

fn consume_structural_node(state: &mut ValidationState<'_>, path: &str) -> Result<(), IrError> {
    state.node_count = state.node_count.saturating_add(1);
    if state.node_count > state.limits.max_nodes {
        resource_limit(path, "documentNodes", state.limits.max_nodes)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "test.parser".into(),
            locator: SourceLocator {
                page: Some(1),
                slide: Some(1),
                sheet: Some("Data".into()),
                cell: Some(CellRef { row: 0, column: 0 }),
                bounds: Some(Rect { x: 1.0, y: 2.0, width: 3.0, height: 4.0 }),
                time: Some(TimeRange { start_ms: 1, end_ms: 2 }),
                part: Some("content.xml".into()),
            },
            confidence: Some(0.9),
        }
    }

    fn node(id: &str, block: Block) -> BlockNode {
        BlockNode { id: NodeId(id.into()), block, provenance: provenance() }
    }

    #[allow(clippy::too_many_lines)] // The fixture deliberately spells out every wire variant.
    fn all_nodes_document() -> Document {
        let inlines = vec![
            Inline::Text {
                value: "rich".into(),
                marks: vec![
                    InlineMark::Bold,
                    InlineMark::Italic,
                    InlineMark::Strikethrough,
                    InlineMark::Underline,
                    InlineMark::Superscript,
                ],
            },
            Inline::Text { value: "subscript".into(), marks: vec![InlineMark::Subscript] },
            Inline::Code("code".into()),
            Inline::Link {
                target: "https://example.invalid".into(),
                content: vec![Inline::Text { value: "link".into(), marks: vec![] }],
            },
            Inline::Formula("x^2".into()),
            Inline::FootnoteReference("note".into()),
            Inline::LineBreak,
        ];
        Document {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            metadata: DocumentMetadata {
                title: Some("Contract".into()),
                authors: vec!["Author".into()],
                properties: BTreeMap::from([("source.kind".into(), "fixture".into())]),
            },
            blocks: vec![
                node("paragraph", Block::Paragraph(inlines)),
                node(
                    "heading",
                    Block::Heading {
                        level: 2,
                        content: vec![Inline::Text { value: "Heading".into(), marks: vec![] }],
                    },
                ),
                node(
                    "list",
                    Block::List {
                        kind: ListKind::Task,
                        start: 1,
                        items: vec![ListItem {
                            checked: Some(true),
                            marker_label: Some("[x]".into()),
                            blocks: vec![node(
                                "nested-list",
                                Block::List {
                                    kind: ListKind::Bullet,
                                    start: 1,
                                    items: vec![ListItem {
                                        checked: None,
                                        marker_label: None,
                                        blocks: vec![node(
                                            "list-text",
                                            Block::Paragraph(vec![Inline::Text {
                                                value: "item".into(),
                                                marks: vec![],
                                            }]),
                                        )],
                                    }],
                                },
                            )],
                        }],
                    },
                ),
                node(
                    "table",
                    Block::Table {
                        rows: vec![TableRow {
                            cells: vec![Cell {
                                row_span: 1,
                                column_span: 2,
                                header: true,
                                blocks: vec![node("cell", Block::Paragraph(vec![]))],
                            }],
                        }],
                    },
                ),
                node(
                    "code",
                    Block::Code { language: Some("rust".into()), text: "fn main() {}".into() },
                ),
                node("formula", Block::Formula("E=mc^2".into())),
                node(
                    "footnote",
                    Block::Footnote {
                        label: "note".into(),
                        blocks: vec![node("footnote-text", Block::Paragraph(vec![]))],
                    },
                ),
                node(
                    "image",
                    Block::Image { asset: AssetId("image-1".into()), alt: Some("diagram".into()) },
                ),
                node(
                    "page",
                    Block::Page {
                        number: 1,
                        blocks: vec![node("page-text", Block::Paragraph(vec![]))],
                    },
                ),
                node(
                    "slide",
                    Block::Slide {
                        number: 1,
                        title: Some("Slide".into()),
                        blocks: vec![node("slide-text", Block::Paragraph(vec![]))],
                    },
                ),
                node(
                    "sheet",
                    Block::Sheet {
                        name: "Data".into(),
                        blocks: vec![node("sheet-text", Block::Paragraph(vec![]))],
                    },
                ),
                node(
                    "timed",
                    Block::TimedSegment {
                        range: TimeRange { start_ms: 10, end_ms: 20 },
                        speaker: Some("Speaker".into()),
                        content: vec![Inline::Text { value: "transcript".into(), marks: vec![] }],
                    },
                ),
                node("rule", Block::Rule),
            ],
        }
    }

    #[test]
    fn every_node_round_trips_through_stable_json() {
        let document = all_nodes_document();
        let json = document.to_json().unwrap();
        assert!(json.contains("\"schemaVersion\":1"));
        for kind in [
            "paragraph",
            "heading",
            "list",
            "table",
            "code",
            "formula",
            "footnote",
            "image",
            "page",
            "slide",
            "sheet",
            "timedSegment",
            "rule",
            "text",
            "link",
            "footnoteReference",
        ] {
            assert!(json.contains(&format!("\"type\":\"{kind}\"")), "missing {kind}");
        }
        assert_eq!(Document::from_json(&json).unwrap(), document);
    }

    #[test]
    fn additive_unknown_fields_are_accepted() {
        let json = r#"{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{},"future":true},"blocks":[],"futureRoot":{}}"#;
        assert_eq!(Document::from_json(json).unwrap(), Document::default());
    }

    #[test]
    fn auxiliary_ir_types_and_provenance_kinds_round_trip() {
        let asset = Asset {
            id: AssetId("asset".into()),
            filename: Some("image.png".into()),
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            external_uri: Some("https://example.invalid/image.png".into()),
        };
        let encoded = serde_json::to_string(&asset).unwrap();
        assert!(encoded.contains("\"mediaType\""));
        assert_eq!(serde_json::from_str::<Asset>(&encoded).unwrap(), asset);

        let diagnostic = Diagnostic {
            code: "recovered".into(),
            severity: DiagnosticSeverity::Warning,
            message: "recovered content".into(),
            locator: Some(provenance().locator),
        };
        let encoded = serde_json::to_string(&diagnostic).unwrap();
        assert_eq!(serde_json::from_str::<Diagnostic>(&encoded).unwrap(), diagnostic);

        for kind in [
            ProvenanceKind::NativeParser,
            ProvenanceKind::LocalOcr,
            ProvenanceKind::AiProvider,
            ProvenanceKind::Metadata,
            ProvenanceKind::Postprocessor,
        ] {
            let encoded = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<ProvenanceKind>(&encoded).unwrap(), kind);
        }
    }

    #[test]
    fn invalid_json_and_schema_versions_have_stable_codes() {
        assert_eq!(Document::from_json("{").unwrap_err().code, IrErrorCode::InvalidJson);
        assert_eq!(
            Document::from_json(r#"{"schemaVersion":2,"futureShape":true}"#).unwrap_err().code,
            IrErrorCode::UnsupportedSchemaVersion
        );
        let document =
            Document { schema_version: DOCUMENT_SCHEMA_VERSION + 1, ..Document::default() };
        assert_eq!(document.validate().unwrap_err().code, IrErrorCode::UnsupportedSchemaVersion);
    }

    #[test]
    fn invalid_nodes_and_provenance_return_paths_without_panicking() {
        let mut document = all_nodes_document();
        document.blocks[0].provenance.confidence = Some(f32::NAN);
        let error = document.validate().unwrap_err();
        assert_eq!(error.code, IrErrorCode::InvalidProvenance);
        assert_eq!(error.path, "$.blocks[0].provenance.confidence");

        let mut document = all_nodes_document();
        document.blocks[0].provenance.locator.page = Some(0);
        assert_eq!(document.validate().unwrap_err().code, IrErrorCode::InvalidLocator);

        let mut document = all_nodes_document();
        document.blocks[1].id = document.blocks[0].id.clone();
        assert_eq!(document.validate().unwrap_err().code, IrErrorCode::DuplicateNodeId);

        let mut document = all_nodes_document();
        document.blocks[1].block = Block::Heading { level: 0, content: Vec::new() };
        assert_eq!(document.validate().unwrap_err().code, IrErrorCode::InvalidNode);
    }

    #[test]
    fn dangling_footnotes_and_invalid_container_values_are_rejected() {
        let mut document = all_nodes_document();
        document.blocks.retain(|node| node.id.0 != "footnote");
        assert_eq!(document.validate().unwrap_err().code, IrErrorCode::InvalidNode);

        let invalid_blocks = [
            Block::List { kind: ListKind::Bullet, start: 1, items: vec![] },
            Block::Table { rows: vec![] },
            Block::Formula(String::new()),
            Block::Image { asset: AssetId(String::new()), alt: None },
            Block::Page { number: 0, blocks: vec![] },
            Block::Slide { number: 0, title: None, blocks: vec![] },
            Block::Sheet { name: String::new(), blocks: vec![] },
            Block::TimedSegment {
                range: TimeRange { start_ms: 2, end_ms: 2 },
                speaker: None,
                content: vec![],
            },
        ];
        for block in invalid_blocks {
            let document = Document { blocks: vec![node("invalid", block)], ..Document::default() };
            assert_eq!(document.validate().unwrap_err().code, IrErrorCode::InvalidNode);
        }
    }

    #[test]
    fn part_names_are_safe_container_relative_paths() {
        for part in [
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "META-INF/manifest.xml",
            "[Content_Types].xml",
        ] {
            let mut document =
                Document { blocks: vec![node("part", Block::Rule)], ..Document::default() };
            document.blocks[0].provenance.locator.part = Some(part.into());
            assert!(document.validate().is_ok(), "rejected safe part {part}");
        }

        for part in [
            "",
            "/word/document.xml",
            "//server/share.xml",
            "C:/word/document.xml",
            "word\\document.xml",
            "word//document.xml",
            "word/./document.xml",
            "word/../document.xml",
            "word/document.xml/",
            "word/\0document.xml",
            "word/\ndocument.xml",
        ] {
            let mut document =
                Document { blocks: vec![node("part", Block::Rule)], ..Document::default() };
            document.blocks[0].provenance.locator.part = Some(part.into());
            let error = document.validate().unwrap_err();
            assert_eq!(error.code, IrErrorCode::InvalidLocator, "accepted unsafe part {part:?}");
            assert_eq!(error.path, "$.blocks[0].provenance.locator.part");
        }
    }

    #[test]
    fn structural_budgets_return_stable_errors() {
        let mut nested = node("depth-0", Block::Paragraph(vec![]));
        for level in 1..=3 {
            nested =
                node(&format!("depth-{level}"), Block::Page { number: 1, blocks: vec![nested] });
        }
        let document = Document { blocks: vec![nested], ..Document::default() };
        let limits = ValidationLimits { max_depth: 3, ..ValidationLimits::default() };
        let error = document.validate_with_limits(&limits).unwrap_err();
        assert_eq!(error.code, IrErrorCode::ResourceLimit);
        assert_eq!(error.code.as_str(), "resourceLimit");
        assert!(error.path.contains(".blocks"));
        let json = document.to_json().unwrap();
        assert_eq!(
            Document::from_json_with_limits(&json, &limits).unwrap_err().code,
            IrErrorCode::ResourceLimit
        );

        let mut default_limited = node("default-depth-0", Block::Paragraph(vec![]));
        for level in 1..=MAX_DOCUMENT_DEPTH {
            default_limited = node(
                &format!("default-depth-{level}"),
                Block::Page { number: 1, blocks: vec![default_limited] },
            );
        }
        let default_limited = Document { blocks: vec![default_limited], ..Document::default() };
        assert_eq!(default_limited.validate().unwrap_err().code, IrErrorCode::ResourceLimit);
        let json = serde_json::to_string(&default_limited).unwrap();
        assert_eq!(Document::from_json(&json).unwrap_err().code, IrErrorCode::ResourceLimit);

        let document = Document {
            blocks: vec![node("one", Block::Rule), node("two", Block::Rule)],
            ..Document::default()
        };
        let limits = ValidationLimits { max_nodes: 1, ..ValidationLimits::default() };
        assert_eq!(
            document.validate_with_limits(&limits).unwrap_err().code,
            IrErrorCode::ResourceLimit
        );

        let document = Document {
            blocks: vec![node(
                "inline",
                Block::Paragraph(vec![Inline::LineBreak, Inline::LineBreak]),
            )],
            ..Document::default()
        };
        let limits = ValidationLimits { max_inlines: 1, ..ValidationLimits::default() };
        assert_eq!(
            document.validate_with_limits(&limits).unwrap_err().code,
            IrErrorCode::ResourceLimit
        );

        let json = Document::default().to_json().unwrap();
        let limits =
            ValidationLimits { max_json_bytes: json.len() - 1, ..ValidationLimits::default() };
        assert_eq!(
            Document::from_json_with_limits(&json, &limits).unwrap_err().code,
            IrErrorCode::ResourceLimit
        );
    }

    #[test]
    fn wire_preflight_rejects_limits_before_typed_deserialization() {
        let node_limited = Document {
            blocks: vec![node("one", Block::Rule), node("two", Block::Rule)],
            ..Document::default()
        };
        let depth_limited = Document {
            blocks: vec![node(
                "outer",
                Block::Page { number: 1, blocks: vec![node("inner", Block::Paragraph(vec![]))] },
            )],
            ..Document::default()
        };
        let inline_limited = Document {
            blocks: vec![node(
                "inline",
                Block::Paragraph(vec![Inline::LineBreak, Inline::LineBreak]),
            )],
            ..Document::default()
        };
        let cases = [
            (
                serde_json::to_value(node_limited).unwrap(),
                ValidationLimits { max_nodes: 1, ..ValidationLimits::default() },
            ),
            (
                serde_json::to_value(depth_limited).unwrap(),
                ValidationLimits { max_depth: 1, ..ValidationLimits::default() },
            ),
            (
                serde_json::to_value(inline_limited).unwrap(),
                ValidationLimits { max_inlines: 1, ..ValidationLimits::default() },
            ),
        ];
        for (value, limits) in cases {
            let error = preflight_document_value(&value, &limits).unwrap_err();
            assert_eq!(error.code, IrErrorCode::ResourceLimit);
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(
                Document::from_json_with_limits(&json, &limits).unwrap_err().code,
                IrErrorCode::ResourceLimit
            );
        }
    }

    fn cell(row_span: u32, column_span: u32) -> Cell {
        Cell { row_span, column_span, header: false, blocks: vec![] }
    }

    fn table_document(rows: Vec<TableRow>) -> Document {
        Document { blocks: vec![node("table-grid", Block::Table { rows })], ..Document::default() }
    }

    #[test]
    fn table_grid_accepts_consistent_column_and_row_spans() {
        let document = table_document(vec![
            TableRow { cells: vec![cell(2, 1), cell(1, 2)] },
            TableRow { cells: vec![cell(1, 2)] },
            TableRow { cells: vec![cell(1, 3)] },
        ]);
        assert!(document.validate().is_ok());

        let fully_spanned_row =
            table_document(vec![TableRow { cells: vec![cell(2, 1)] }, TableRow { cells: vec![] }]);
        assert!(fully_spanned_row.validate().is_ok());
    }

    #[test]
    fn table_grid_rejects_inconsistent_width_overlap_and_out_of_bounds_spans() {
        let invalid_tables = [
            table_document(vec![
                TableRow { cells: vec![cell(1, 2)] },
                TableRow { cells: vec![cell(1, 1)] },
            ]),
            table_document(vec![
                TableRow { cells: vec![cell(1, 1), cell(2, 1)] },
                TableRow { cells: vec![cell(1, 2)] },
            ]),
            table_document(vec![
                TableRow { cells: vec![cell(1, 2)] },
                TableRow { cells: vec![cell(1, 3)] },
            ]),
            table_document(vec![TableRow { cells: vec![cell(2, 1)] }]),
        ];
        for document in invalid_tables {
            assert_eq!(document.validate().unwrap_err().code, IrErrorCode::InvalidNode);
        }

        let document = table_document(vec![TableRow { cells: vec![cell(1, 3)] }]);
        let limits = ValidationLimits { max_table_columns: 2, ..ValidationLimits::default() };
        assert_eq!(
            document.validate_with_limits(&limits).unwrap_err().code,
            IrErrorCode::ResourceLimit
        );
    }

    #[test]
    fn wire_preflight_rejects_wide_tables_and_defers_malformed_shapes() {
        let wide = table_document(vec![TableRow { cells: vec![cell(1, 3)] }]);
        let value = serde_json::to_value(wide).unwrap();
        let limits = ValidationLimits { max_table_columns: 2, ..ValidationLimits::default() };
        let error = preflight_document_value(&value, &limits).unwrap_err();
        assert_eq!(error.code, IrErrorCode::ResourceLimit);
        assert!(error.path.ends_with(".columnSpan"));

        let malformed = serde_json::json!({
            "schemaVersion": DOCUMENT_SCHEMA_VERSION,
            "metadata": { "title": null, "authors": [], "properties": {} },
            "blocks": { "not": "an array" }
        });
        assert!(preflight_document_value(&malformed, &ValidationLimits::default()).is_ok());
        let json = serde_json::to_string(&malformed).unwrap();
        assert_eq!(Document::from_json(&json).unwrap_err().code, IrErrorCode::InvalidJson);
    }
}
