use into_markdown_core::{ProvenanceKind, ResourceReservation};
use serde::{Deserialize, Serialize};

/// Stable schema version for semantic layout authorities and reports.
pub const AUTHORITY_SCHEMA_VERSION: u32 = 1;

/// Source cohort used to select the required semantic precision and recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityCohort {
    /// Deterministic text and modern package formats.
    Modern,
    /// Geometry-derived PDF, legacy Office, or image OCR output.
    GeometryDerived,
}

/// Precision and recall floor expressed in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityThreshold {
    /// Minimum accepted precision in `0..=10_000`.
    pub minimum_precision_basis_points: u16,
    /// Minimum accepted recall in `0..=10_000`.
    pub minimum_recall_basis_points: u16,
}

impl QualityThreshold {
    /// Required threshold for a source cohort.
    #[must_use]
    pub const fn for_cohort(cohort: QualityCohort) -> Self {
        match cohort {
            QualityCohort::Modern => {
                Self { minimum_precision_basis_points: 9_500, minimum_recall_basis_points: 9_500 }
            }
            QualityCohort::GeometryDerived => {
                Self { minimum_precision_basis_points: 9_000, minimum_recall_basis_points: 9_000 }
            }
        }
    }
}

/// One hash-pinned semantic-layout authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixtureAuthority {
    /// Authority schema version.
    pub schema_version: u32,
    /// Repository-stable fixture identifier.
    pub fixture_id: String,
    /// Product format or explicitly named format family.
    pub format: String,
    /// Source cohort controlling the threshold floor.
    pub cohort: QualityCohort,
    /// Permitted absolute geometry drift in thousandths of a source unit.
    pub geometry_tolerance_milli: u32,
    /// Expected semantic structure.
    pub snapshot: SemanticSnapshot,
    /// SHA-256 of canonical validated IR JSON.
    pub ir_sha256: String,
    /// SHA-256 of the exact GFM output.
    pub gfm_sha256: String,
}

/// Deterministic cross-format projection of the document IR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticSnapshot {
    /// Material nodes in depth-first reading order.
    pub nodes: Vec<SemanticNode>,
    /// Stable asset inventory in asset-ID order.
    pub assets: Vec<AssetSnapshot>,
}

/// A semantic projection that retains its request-memory accounting lease.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticProjection {
    /// Projected semantic structure.
    pub snapshot: SemanticSnapshot,
    #[serde(skip)]
    pub(crate) memory_lease: ResourceReservation,
}

impl SemanticProjection {
    /// Borrow the projected structure while its request accounting remains live.
    #[must_use]
    pub const fn snapshot(&self) -> &SemanticSnapshot {
        &self.snapshot
    }

    /// Consume an authority-generation projection.
    ///
    /// This deliberately releases request accounting and is intended only for
    /// checked-in golden generation, not request processing.
    #[must_use]
    pub fn into_authority_snapshot(self) -> SemanticSnapshot {
        self.snapshot
    }

    /// Projections retain their request-memory accounting until dropped.
    #[doc(hidden)]
    #[must_use]
    pub fn retained_memory_is_accounted(&self) -> bool {
        let _ = &self.memory_lease;
        true
    }
}

/// One semantic node in reading order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticNode {
    /// Stable document node ID.
    pub id: String,
    /// Cross-format semantic kind.
    pub kind: String,
    /// Parent node ID; root-level nodes have no parent.
    pub parent_id: Option<String>,
    /// Zero-based global reading-order position.
    pub order: u64,
    /// Zero-based sibling position within the structural parent.
    pub sibling_order: u64,
    /// Structural nesting depth, with root nodes at zero.
    pub depth: u16,
    /// Normalized textual payload relevant to the semantic kind.
    pub text: String,
    /// Page, slide, sheet, cell, part, and source byte boundary.
    pub boundary: SourceBoundary,
    /// Quantized source rectangle.
    pub bounds: Option<NormalizedBounds>,
    /// Ordered node/inline/OCR source chain.
    pub source_chain: Vec<SourceStep>,
    /// Exact logical table topology for table nodes.
    pub table: Option<TableTopology>,
    /// References owned by this node.
    pub references: Vec<SemanticReference>,
}

/// Stable source boundary retained independently of presentation formatting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceBoundary {
    /// One-based page.
    pub page: Option<u32>,
    /// One-based slide.
    pub slide: Option<u32>,
    /// Worksheet name.
    pub sheet: Option<String>,
    /// Cell address as `row,column` using zero-based coordinates.
    pub cell: Option<String>,
    /// Safe package part.
    pub part: Option<String>,
    /// Inclusive byte start.
    pub byte_start: Option<u64>,
    /// Exclusive byte end.
    pub byte_end: Option<u64>,
}

/// Finite rectangle quantized to thousandths of a source unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedBounds {
    /// Left edge multiplied by 1000 and rounded.
    pub x_milli: i64,
    /// Top edge multiplied by 1000 and rounded.
    pub y_milli: i64,
    /// Width multiplied by 1000 and rounded.
    pub width_milli: i64,
    /// Height multiplied by 1000 and rounded.
    pub height_milli: i64,
}

/// One stable provider step in a node's source chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceStep {
    /// Provenance class.
    pub kind: ProvenanceKind,
    /// Stable provider or model implementation ID.
    pub provider: String,
}

/// Exact logical table shape, including origin-cell spans and nested block IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableTopology {
    /// Logical row count.
    pub rows: u64,
    /// Maximum occupied logical column count.
    pub columns: u64,
    /// Origin cells in row-major order.
    pub cells: Vec<TableCellTopology>,
}

/// One table origin cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableCellTopology {
    /// Zero-based logical row.
    pub row: u64,
    /// Zero-based logical column.
    pub column: u64,
    /// Row span.
    pub row_span: u32,
    /// Column span.
    pub column_span: u32,
    /// Whether the cell is a header.
    pub header: bool,
    /// Stable IDs of direct nested blocks.
    pub block_ids: Vec<String>,
}

/// A body-to-resource or footnote association.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticReference {
    /// Reference family: `asset`, `footnote`, or `link`.
    pub kind: String,
    /// Stable target.
    pub target: String,
}

/// Asset metadata without resource bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetSnapshot {
    /// Stable asset ID.
    pub id: String,
    /// Media type.
    pub media_type: String,
    /// Suggested filename.
    pub filename: Option<String>,
    /// Original external URI.
    pub external_uri: Option<String>,
    /// Byte length.
    pub bytes: u64,
    /// Complete SHA-256 of resource bytes; external-only assets hash an empty byte slice.
    pub sha256: String,
    /// Whether a material body node references this asset.
    pub referenced: bool,
}

/// Stable semantic difference category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffKind {
    /// An expected semantic node is absent.
    Missing,
    /// A stable node or asset ID occurs more than once.
    Duplicate,
    /// An unexpected semantic node is present.
    Unexpected,
    /// Relative reading order changed.
    Order,
    /// Semantic kind or normalized textual content changed.
    Content,
    /// Parent, depth, or sibling position changed.
    Hierarchy,
    /// Page, slide, sheet, cell, part, or byte boundary changed.
    Boundary,
    /// Geometry moved outside the configured tolerance.
    Geometry,
    /// Table rows, columns, spans, headers, or nested blocks changed.
    TableTopology,
    /// Resource, link, or footnote association changed.
    ResourceAssociation,
    /// Ordered source/provider chain changed.
    SourceChain,
    /// Canonical IR JSON changed.
    IrGolden,
    /// Exact GFM changed.
    GfmGolden,
    /// Precision or recall dropped below the cohort floor.
    Threshold,
}

/// One deterministic, source-locatable quality difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QualityDiff {
    /// Difference category.
    pub kind: DiffKind,
    /// Fixture that failed.
    pub fixture_id: String,
    /// Node or asset ID when applicable.
    pub node_id: Option<String>,
    /// Human-readable page/slide/sheet and node position.
    pub location: String,
    /// Compact expected value.
    pub expected: Option<String>,
    /// Compact actual value.
    pub actual: Option<String>,
}

/// Aggregate semantic precision/recall counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityMetrics {
    /// Correct nodes matched by ID and semantic kind.
    pub true_positive: u64,
    /// Unexpected or wrong-kind nodes.
    pub false_positive: u64,
    /// Missing or wrong-kind nodes.
    pub false_negative: u64,
    /// Precision in basis points.
    pub precision_basis_points: u16,
    /// Recall in basis points.
    pub recall_basis_points: u16,
}

/// Complete quality result. The request-memory lease lives until this report is dropped.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityReport {
    /// Fixture identifier.
    pub fixture_id: String,
    /// Whether every structural, hash, and threshold check passed.
    pub passed: bool,
    /// Semantic precision/recall.
    pub metrics: QualityMetrics,
    /// Actual canonical IR digest.
    pub ir_sha256: String,
    /// Actual GFM digest.
    pub gfm_sha256: String,
    /// Stable differences in category/location order.
    pub diffs: Vec<QualityDiff>,
    #[serde(skip)]
    pub(crate) memory_lease: ResourceReservation,
}

impl QualityReport {
    /// Reports retain their request-memory accounting until dropped.
    #[doc(hidden)]
    #[must_use]
    pub fn retained_memory_is_accounted(&self) -> bool {
        let _ = &self.memory_lease;
        true
    }
}
