//! Deterministic, format-independent semantic-layout regression auditing.
//!
//! Converters remain responsible for source-authoritative facts. This module
//! compares already validated Document IR and never repairs converter output.

mod geometry;
mod paragraph_list;
mod quality_metrics;
mod reading_order;
mod resource_association;
mod table;

pub use table::TableTopology;

#[cfg(test)]
mod tests;

use crate::{
    Asset, Block, BlockNode, ConversionError, Document, ExecutionContext, Inline, Rect,
    ResourceReservation,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const CHECKPOINT_NODES: usize = 128;
const NODE_HIGH_WATER_BYTES: u64 = 1_536;
const ASSET_HIGH_WATER_BYTES: u64 = 512;
const REPORT_BASE_BYTES: u64 = 16 * 1024;
const FIELD_COPY_HIGH_WATER: u64 = 8;

/// Source boundary containing one semantic node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum LayoutBoundary {
    /// One-based source page.
    Page(u32),
    /// One-based presentation slide.
    Slide(u32),
    /// Worksheet name.
    Sheet(String),
}

/// Stable semantic-layout difference category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayoutDiffKind {
    /// A golden semantic node is absent.
    Missing,
    /// A semantic node ID occurs more than once in the observed traversal.
    Duplicate,
    /// Matching nodes occur in a different reading order.
    OutOfOrder,
    /// A node has the wrong semantic parent or boundary.
    WrongHierarchy,
    /// A table has different row/column/span topology.
    TableTopology,
    /// An image or other resource is missing, duplicated, or orphaned.
    ResourceAssociation,
    /// Source geometry exceeds the authority's numeric tolerance.
    Geometry,
    /// A page, slide, or worksheet boundary differs.
    Boundary,
    /// The observed node has no golden counterpart.
    Unexpected,
}

/// One path-addressed deterministic layout difference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutDiff {
    /// Difference category.
    pub kind: LayoutDiffKind,
    /// Stable node or asset ID, when available.
    pub node: Option<String>,
    /// Page, slide, or worksheet containing the difference.
    pub boundary: Option<LayoutBoundary>,
    /// Expected canonical value.
    pub expected: String,
    /// Observed canonical value.
    pub actual: String,
}

/// Precision and recall over semantic nodes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticMetrics {
    /// Correct observed nodes divided by all observed nodes.
    pub precision: f64,
    /// Correct observed nodes divided by all golden nodes.
    pub recall: f64,
    /// Number of matching semantic nodes.
    pub true_positive: u64,
    /// Number of unexpected semantic nodes.
    pub false_positive: u64,
    /// Number of missing semantic nodes.
    pub false_negative: u64,
}

/// Authority-controlled comparison policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutQualityConfig {
    /// Allowed absolute difference for normalized source coordinates.
    pub coordinate_tolerance: f32,
    /// Minimum node precision required by the gate.
    pub minimum_precision: f64,
    /// Minimum node recall required by the gate.
    pub minimum_recall: f64,
    /// Maximum UTF-8 bytes in any cloned ID, text, sheet name, or asset ID.
    pub max_field_bytes: u64,
}

impl LayoutQualityConfig {
    fn validate(self) -> Result<(), ConversionError> {
        if !self.coordinate_tolerance.is_finite() || self.coordinate_tolerance < 0.0 {
            return Err(invalid("coordinate tolerance must be finite and non-negative"));
        }
        if !self.minimum_precision.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_precision)
            || !self.minimum_recall.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_recall)
        {
            return Err(invalid("precision and recall thresholds must be within 0..=1"));
        }
        if self.max_field_bytes == 0 {
            return Err(limit_field("field byte limit must be positive"));
        }
        Ok(())
    }
}

/// Complete fixture-scoped quality report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutQualityReport {
    /// Fixture ID from the hash-bound authority.
    pub fixture: String,
    /// Semantic-node metrics.
    pub metrics: SemanticMetrics,
    /// Required thresholds copied into the report.
    pub minimum_precision: f64,
    /// Required recall copied into the report.
    pub minimum_recall: f64,
    /// Sorted differences, stable across platforms.
    pub differences: Vec<LayoutDiff>,
}

impl LayoutQualityReport {
    /// Whether all thresholds and exact structural checks passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.metrics.precision >= self.minimum_precision
            && self.metrics.recall >= self.minimum_recall
            && self.differences.is_empty()
    }
}

/// A quality report retaining its request-scoped memory lease.
pub struct LayoutAudit {
    report: LayoutQualityReport,
    _reservation: ResourceReservation,
}

impl LayoutAudit {
    /// Borrow the deterministic report while its memory remains accounted.
    #[must_use]
    pub const fn report(&self) -> &LayoutQualityReport {
        &self.report
    }

    /// Serialize the report using stable field and difference ordering.
    ///
    /// # Errors
    ///
    /// Returns an internal error if serialization of the fixed report schema fails.
    pub fn to_json(&self) -> Result<String, ConversionError> {
        serde_json::to_string(&self.report)
            .map_err(|error| invalid(format!("serialize semantic layout report: {error}")))
    }
}

/// One normalized semantic node in an independent layout golden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutGoldenNode {
    /// Stable document-scoped node ID.
    pub id: String,
    /// Canonical block kind.
    pub kind: String,
    /// Canonical semantic text.
    pub text: String,
    /// Semantic parent node ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Page, slide, or sheet boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<LayoutBoundary>,
    /// Source geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    /// Exact table topology, when this node is a table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<table::TableTopology>,
    /// Referenced resource ID, when this node is an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
}

type SemanticNode = LayoutGoldenNode;

/// Versioned normalized semantic-layout golden independent of converter output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutGolden {
    /// Golden schema version.
    pub schema_version: u32,
    /// Nodes in canonical reading order.
    pub nodes: Vec<LayoutGoldenNode>,
    /// Published asset IDs in canonical order.
    pub assets: Vec<String>,
}

/// A normalized golden retaining its request-scoped memory lease.
pub struct LayoutGoldenAudit {
    golden: LayoutGolden,
    _reservation: ResourceReservation,
}

impl LayoutGoldenAudit {
    /// Borrow the normalized golden while its memory remains accounted.
    #[must_use]
    pub const fn golden(&self) -> &LayoutGolden {
        &self.golden
    }

    /// Serialize the normalized golden deterministically.
    ///
    /// # Errors
    ///
    /// Returns an internal error if serialization fails.
    pub fn to_json(&self) -> Result<String, ConversionError> {
        serde_json::to_string(&self.golden)
            .map_err(|error| invalid(format!("serialize semantic layout golden: {error}")))
    }
}

/// Compare observed IR/resources with hash-bound golden IR/resources.
///
/// The comparison is linearithmic, checkpoints cooperative cancellation and
/// timeout, validates both documents before allocating the report, and retains
/// one memory lease until the returned audit is dropped. No partial report is
/// published on a resource or execution failure.
///
/// # Errors
///
/// Returns a controlled validation, resource-limit, cancellation, or timeout
/// error before publishing any report.
pub fn audit_semantic_layout(
    fixture: &str,
    actual: &Document,
    actual_assets: &[Asset],
    golden: &Document,
    golden_assets: &[Asset],
    config: LayoutQualityConfig,
    context: &ExecutionContext,
) -> Result<LayoutAudit, ConversionError> {
    context.checkpoint()?;
    config.validate()?;
    if let Err(error) = actual.validate()
        && error.code != crate::IrErrorCode::DuplicateNodeId
    {
        return Err(invalid(format!("actual IR: {error}")));
    }
    golden.validate().map_err(|error| invalid(format!("golden IR: {error}")))?;
    let mut actual_plan = ClonePlan::default();
    plan_nodes(&actual.blocks, None, None, config.max_field_bytes, context, &mut actual_plan)?;
    let mut golden_plan = ClonePlan::default();
    plan_nodes(&golden.blocks, None, None, config.max_field_bytes, context, &mut golden_plan)?;
    let total_nodes =
        actual_plan.nodes.checked_add(golden_plan.nodes).ok_or_else(working_overflow)?;
    let total_assets =
        actual_assets.len().checked_add(golden_assets.len()).ok_or_else(working_overflow)?;
    let fixture_bytes = checked_field(fixture.len(), config.max_field_bytes)?;
    let mut field_bytes = actual_plan
        .field_bytes
        .checked_add(golden_plan.field_bytes)
        .and_then(|value| value.checked_add(fixture_bytes))
        .ok_or_else(working_overflow)?;
    for asset in actual_assets.iter().chain(golden_assets) {
        field_bytes = field_bytes
            .checked_add(checked_field(asset.id.0.len(), config.max_field_bytes)?)
            .ok_or_else(working_overflow)?;
    }
    let bytes = u64::try_from(total_nodes)
        .ok()
        .and_then(|value| value.checked_mul(NODE_HIGH_WATER_BYTES))
        .and_then(|value| {
            value
                .checked_add(u64::try_from(total_assets).ok()?.checked_mul(ASSET_HIGH_WATER_BYTES)?)
        })
        .and_then(|value| value.checked_add(field_bytes.checked_mul(FIELD_COPY_HIGH_WATER)?))
        .and_then(|value| value.checked_add(REPORT_BASE_BYTES))
        .ok_or_else(working_overflow)?;
    let reservation = context.reserve_memory(bytes)?;

    let actual_nodes = collect_nodes(&actual.blocks, actual_plan.nodes, context)?;
    let golden_nodes = collect_nodes(&golden.blocks, golden_plan.nodes, context)?;
    let mut differences = Vec::new();
    reading_order::compare(&golden_nodes, &actual_nodes, &mut differences);
    paragraph_list::compare(&golden_nodes, &actual_nodes, &mut differences);
    table::compare(&golden_nodes, &actual_nodes, &mut differences);
    geometry::compare(&golden_nodes, &actual_nodes, config.coordinate_tolerance, &mut differences);
    resource_association::compare(
        &golden_nodes,
        golden_assets,
        &actual_nodes,
        actual_assets,
        &mut differences,
    );
    differences.sort();
    differences.dedup();
    context.checkpoint()?;
    let metrics = quality_metrics::metrics(&golden_nodes, &actual_nodes);
    Ok(LayoutAudit {
        report: LayoutQualityReport {
            fixture: fixture.to_owned(),
            metrics,
            minimum_precision: config.minimum_precision,
            minimum_recall: config.minimum_recall,
            differences,
        },
        _reservation: reservation,
    })
}

/// Normalize validated IR and published resources into a portable golden.
///
/// # Errors
///
/// Returns a controlled validation, resource-limit, cancellation, or timeout
/// error without publishing a partial golden.
pub fn capture_semantic_layout_golden(
    document: &Document,
    assets: &[Asset],
    config: LayoutQualityConfig,
    context: &ExecutionContext,
) -> Result<LayoutGoldenAudit, ConversionError> {
    context.checkpoint()?;
    config.validate()?;
    document.validate().map_err(|error| invalid(format!("golden source IR: {error}")))?;
    let mut plan = ClonePlan::default();
    plan_nodes(&document.blocks, None, None, config.max_field_bytes, context, &mut plan)?;
    for asset in assets {
        plan.field_bytes = plan
            .field_bytes
            .checked_add(checked_field(asset.id.0.len(), config.max_field_bytes)?)
            .ok_or_else(working_overflow)?;
    }
    let bytes = planned_bytes(plan.nodes, assets.len(), plan.field_bytes)?;
    let reservation = context.reserve_memory(bytes)?;
    let nodes = collect_nodes(&document.blocks, plan.nodes, context)?;
    let mut golden_assets = Vec::new();
    golden_assets.try_reserve_exact(assets.len()).map_err(|_| working_overflow())?;
    golden_assets.extend(assets.iter().map(|asset| asset.id.0.clone()));
    golden_assets.sort();
    context.checkpoint()?;
    Ok(LayoutGoldenAudit {
        golden: LayoutGolden { schema_version: 1, nodes, assets: golden_assets },
        _reservation: reservation,
    })
}

/// Compare observed IR/resources with an independent normalized layout golden.
///
/// # Errors
///
/// Returns a controlled validation, golden-schema, resource-limit,
/// cancellation, or timeout error without publishing a partial report.
pub fn audit_semantic_layout_golden(
    fixture: &str,
    actual: &Document,
    actual_assets: &[Asset],
    golden: &LayoutGolden,
    config: LayoutQualityConfig,
    context: &ExecutionContext,
) -> Result<LayoutAudit, ConversionError> {
    context.checkpoint()?;
    config.validate()?;
    if golden.schema_version != 1 {
        return Err(invalid(format!(
            "unsupported semantic layout golden schema {}",
            golden.schema_version
        )));
    }
    if let Err(error) = actual.validate()
        && error.code != crate::IrErrorCode::DuplicateNodeId
    {
        return Err(invalid(format!("actual IR: {error}")));
    }
    let mut actual_plan = ClonePlan::default();
    plan_nodes(&actual.blocks, None, None, config.max_field_bytes, context, &mut actual_plan)?;
    let golden_fields = plan_golden(golden, config.max_field_bytes, context)?;
    let fixture_bytes = checked_field(fixture.len(), config.max_field_bytes)?;
    let field_bytes = actual_plan
        .field_bytes
        .checked_add(golden_fields)
        .and_then(|value| value.checked_add(fixture_bytes))
        .ok_or_else(working_overflow)?;
    let node_count =
        actual_plan.nodes.checked_add(golden.nodes.len()).ok_or_else(working_overflow)?;
    let asset_count =
        actual_assets.len().checked_add(golden.assets.len()).ok_or_else(working_overflow)?;
    let reservation =
        context.reserve_memory(planned_bytes(node_count, asset_count, field_bytes)?)?;
    let actual_nodes = collect_nodes(&actual.blocks, actual_plan.nodes, context)?;
    let mut differences = Vec::new();
    reading_order::compare(&golden.nodes, &actual_nodes, &mut differences);
    paragraph_list::compare(&golden.nodes, &actual_nodes, &mut differences);
    table::compare(&golden.nodes, &actual_nodes, &mut differences);
    geometry::compare(&golden.nodes, &actual_nodes, config.coordinate_tolerance, &mut differences);
    resource_association::compare_golden(
        &golden.nodes,
        &golden.assets,
        &actual_nodes,
        actual_assets,
        &mut differences,
    );
    differences.sort();
    differences.dedup();
    context.checkpoint()?;
    let metrics = quality_metrics::metrics(&golden.nodes, &actual_nodes);
    Ok(LayoutAudit {
        report: LayoutQualityReport {
            fixture: fixture.to_owned(),
            metrics,
            minimum_precision: config.minimum_precision,
            minimum_recall: config.minimum_recall,
            differences,
        },
        _reservation: reservation,
    })
}

fn plan_golden(
    golden: &LayoutGolden,
    max_field_bytes: u64,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let mut bytes = 0_u64;
    for (index, node) in golden.nodes.iter().enumerate() {
        if index.is_multiple_of(CHECKPOINT_NODES) {
            context.checkpoint()?;
        }
        for field in [
            Some(node.id.as_str()),
            Some(node.kind.as_str()),
            Some(node.text.as_str()),
            node.parent.as_deref(),
            node.asset.as_deref(),
            match &node.boundary {
                Some(LayoutBoundary::Sheet(name)) => Some(name.as_str()),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        {
            bytes = bytes
                .checked_add(checked_field(field.len(), max_field_bytes)?)
                .ok_or_else(working_overflow)?;
        }
    }
    for asset in &golden.assets {
        bytes = bytes
            .checked_add(checked_field(asset.len(), max_field_bytes)?)
            .ok_or_else(working_overflow)?;
    }
    Ok(bytes)
}

fn planned_bytes(nodes: usize, assets: usize, field_bytes: u64) -> Result<u64, ConversionError> {
    u64::try_from(nodes)
        .ok()
        .and_then(|value| value.checked_mul(NODE_HIGH_WATER_BYTES))
        .and_then(|value| {
            value.checked_add(u64::try_from(assets).ok()?.checked_mul(ASSET_HIGH_WATER_BYTES)?)
        })
        .and_then(|value| value.checked_add(field_bytes.checked_mul(FIELD_COPY_HIGH_WATER)?))
        .and_then(|value| value.checked_add(REPORT_BASE_BYTES))
        .ok_or_else(working_overflow)
}

fn collect_nodes(
    roots: &[BlockNode],
    node_count: usize,
    context: &ExecutionContext,
) -> Result<Vec<SemanticNode>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(node_count).map_err(|_| working_overflow())?;
    let mut stack = Vec::new();
    stack.try_reserve_exact(roots.len()).map_err(|_| working_overflow())?;
    for node in roots.iter().rev() {
        stack.push((node, None, None));
    }
    while let Some((node, parent, inherited_boundary)) = stack.pop() {
        if output.len().is_multiple_of(CHECKPOINT_NODES) {
            context.checkpoint()?;
        }
        let own_boundary = boundary(node).or(inherited_boundary.clone());
        let semantic = SemanticNode {
            id: node.id.0.clone(),
            kind: kind(&node.block).into(),
            text: semantic_text(&node.block)?,
            parent: parent.clone(),
            boundary: own_boundary.clone(),
            bounds: node.provenance.locator.bounds,
            table: table::topology(&node.block),
            asset: match &node.block {
                Block::Image { asset, .. } => Some(asset.0.clone()),
                _ => None,
            },
        };
        output.push(semantic);
        let mut children = Vec::new();
        child_nodes(&node.block, &mut children);
        stack.try_reserve(children.len()).map_err(|_| working_overflow())?;
        for child in children.into_iter().rev() {
            stack.push((child, Some(node.id.0.clone()), own_boundary.clone()));
        }
    }
    Ok(output)
}

#[derive(Default)]
struct ClonePlan {
    nodes: usize,
    field_bytes: u64,
}

fn plan_nodes(
    nodes: &[BlockNode],
    parent_id_bytes: Option<u64>,
    inherited_sheet_bytes: Option<u64>,
    max_field_bytes: u64,
    context: &ExecutionContext,
    plan: &mut ClonePlan,
) -> Result<(), ConversionError> {
    for node in nodes {
        plan.nodes = plan.nodes.checked_add(1).ok_or_else(working_overflow)?;
        if plan.nodes.is_multiple_of(CHECKPOINT_NODES) {
            context.checkpoint()?;
        }
        let id_bytes = checked_field(node.id.0.len(), max_field_bytes)?;
        let text_bytes = checked_field(semantic_text_len(&node.block)?, max_field_bytes)?;
        let sheet_bytes = match &node.block {
            Block::Sheet { name, .. } => Some(checked_field(name.len(), max_field_bytes)?),
            _ => inherited_sheet_bytes,
        };
        let asset_bytes = match &node.block {
            Block::Image { asset, .. } => checked_field(asset.0.len(), max_field_bytes)?,
            _ => 0,
        };
        plan.field_bytes = plan
            .field_bytes
            .checked_add(id_bytes)
            .and_then(|value| value.checked_add(text_bytes))
            .and_then(|value| value.checked_add(parent_id_bytes.unwrap_or(0)))
            .and_then(|value| value.checked_add(sheet_bytes.unwrap_or(0)))
            .and_then(|value| value.checked_add(asset_bytes))
            .ok_or_else(working_overflow)?;
        match &node.block {
            Block::List { items, .. } => {
                for item in items {
                    plan_nodes(
                        &item.blocks,
                        Some(id_bytes),
                        sheet_bytes,
                        max_field_bytes,
                        context,
                        plan,
                    )?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter().flat_map(|row| &row.cells) {
                    plan_nodes(
                        &cell.blocks,
                        Some(id_bytes),
                        sheet_bytes,
                        max_field_bytes,
                        context,
                        plan,
                    )?;
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                plan_nodes(blocks, Some(id_bytes), sheet_bytes, max_field_bytes, context, plan)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn child_nodes<'a>(block: &'a Block, output: &mut Vec<&'a BlockNode>) {
    match block {
        Block::List { items, .. } => output.extend(items.iter().flat_map(|item| &item.blocks)),
        Block::Table { rows, .. } => {
            output.extend(rows.iter().flat_map(|row| &row.cells).flat_map(|cell| &cell.blocks));
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => output.extend(blocks),
        _ => {}
    }
}

fn boundary(node: &BlockNode) -> Option<LayoutBoundary> {
    match &node.block {
        Block::Page { number, .. } => Some(LayoutBoundary::Page(*number)),
        Block::Slide { number, .. } => Some(LayoutBoundary::Slide(*number)),
        Block::Sheet { name, .. } => Some(LayoutBoundary::Sheet(name.clone())),
        _ => None,
    }
}

fn kind(block: &Block) -> &'static str {
    match block {
        Block::Paragraph(_) => "paragraph",
        Block::Heading { .. } => "heading",
        Block::List { .. } => "list",
        Block::Table { .. } => "table",
        Block::Code { .. } => "code",
        Block::Formula(_) => "formula",
        Block::Footnote { .. } => "footnote",
        Block::Image { .. } => "image",
        Block::Page { .. } => "page",
        Block::Slide { .. } => "slide",
        Block::Sheet { .. } => "sheet",
        Block::TimedSegment { .. } => "timedSegment",
        Block::Rule => "rule",
    }
}

fn semantic_text(block: &Block) -> Result<String, ConversionError> {
    let capacity = semantic_text_len(block)?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| working_overflow())?;
    match block {
        Block::Paragraph(values) | Block::Heading { content: values, .. } => {
            push_inline_text(values, &mut output);
        }
        Block::Code { text, .. } | Block::Formula(text) => output.push_str(text),
        Block::Footnote { label, .. } => output.push_str(label),
        Block::Image { alt, .. } => output.push_str(alt.as_deref().unwrap_or_default()),
        Block::Page { number, .. } | Block::Slide { number, .. } => {
            use std::fmt::Write as _;
            write!(output, "{number}").map_err(|_| working_overflow())?;
        }
        Block::Sheet { name, .. } => output.push_str(name),
        Block::TimedSegment { content, .. } => push_inline_text(content, &mut output),
        _ => {}
    }
    Ok(output)
}

fn semantic_text_len(block: &Block) -> Result<usize, ConversionError> {
    match block {
        Block::Paragraph(values) | Block::Heading { content: values, .. } => {
            inline_text_len(values)
        }
        Block::Code { text, .. } | Block::Formula(text) => Ok(text.len()),
        Block::Footnote { label, .. } => Ok(label.len()),
        Block::Image { alt, .. } => Ok(alt.as_ref().map_or(0, String::len)),
        Block::Page { number, .. } | Block::Slide { number, .. } => Ok(decimal_len(*number)),
        Block::Sheet { name, .. } => Ok(name.len()),
        Block::TimedSegment { content, .. } => inline_text_len(content),
        _ => Ok(0),
    }
}

fn inline_text_len(values: &[Inline]) -> Result<usize, ConversionError> {
    let mut length = 0_usize;
    for inline in values {
        match inline {
            Inline::Text { value, .. }
            | Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Code(value)
            | Inline::Formula(value)
            | Inline::FootnoteReference(value) => {
                length = length.checked_add(value.len()).ok_or_else(working_overflow)?;
            }
            Inline::Link { content, .. } => {
                length =
                    length.checked_add(inline_text_len(content)?).ok_or_else(working_overflow)?;
            }
            Inline::LineBreak => length = length.checked_add(1).ok_or_else(working_overflow)?,
        }
    }
    Ok(length)
}

fn push_inline_text(values: &[Inline], output: &mut String) {
    for inline in values {
        match inline {
            Inline::Text { value, .. }
            | Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Code(value)
            | Inline::Formula(value)
            | Inline::FootnoteReference(value) => output.push_str(value),
            Inline::Link { content, .. } => push_inline_text(content, output),
            Inline::LineBreak => output.push('\n'),
        }
    }
}

fn decimal_len(mut value: u32) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn by_id(nodes: &[SemanticNode]) -> BTreeMap<&str, &SemanticNode> {
    nodes.iter().map(|node| (node.id.as_str(), node)).collect()
}

fn duplicate_ids(nodes: &[SemanticNode]) -> BTreeSet<&str> {
    let mut seen = BTreeSet::new();
    let mut duplicate = BTreeSet::new();
    for node in nodes {
        if !seen.insert(node.id.as_str()) {
            duplicate.insert(node.id.as_str());
        }
    }
    duplicate
}

fn invalid(detail: impl Into<String>) -> ConversionError {
    ConversionError::Internal { detail: detail.into() }
}

fn working_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "semanticLayoutWorkingSet",
        detail: "semantic layout audit working set overflow".into(),
    }
}

fn checked_field(bytes: usize, maximum: u64) -> Result<u64, ConversionError> {
    let bytes = u64::try_from(bytes).map_err(|_| working_overflow())?;
    if bytes > maximum {
        return Err(limit_field(format!("semantic layout field {bytes} > {maximum}")));
    }
    Ok(bytes)
}

fn limit_field(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "semanticLayoutFieldBytes", detail: detail.into() }
}
