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

#[derive(Debug, Clone, PartialEq)]
struct SemanticNode {
    id: String,
    kind: &'static str,
    text: String,
    parent: Option<String>,
    boundary: Option<LayoutBoundary>,
    bounds: Option<Rect>,
    table: Option<table::TableTopology>,
    asset: Option<String>,
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
    let actual_count = count_nodes(&actual.blocks);
    let golden_count = count_nodes(&golden.blocks);
    let total_nodes = actual_count.checked_add(golden_count).ok_or_else(working_overflow)?;
    let total_assets =
        actual_assets.len().checked_add(golden_assets.len()).ok_or_else(working_overflow)?;
    let bytes = u64::try_from(total_nodes)
        .ok()
        .and_then(|value| value.checked_mul(NODE_HIGH_WATER_BYTES))
        .and_then(|value| {
            value
                .checked_add(u64::try_from(total_assets).ok()?.checked_mul(ASSET_HIGH_WATER_BYTES)?)
        })
        .and_then(|value| value.checked_add(REPORT_BASE_BYTES))
        .ok_or_else(working_overflow)?;
    let reservation = context.reserve_memory(bytes)?;

    let actual_nodes = collect_nodes(&actual.blocks, context)?;
    let golden_nodes = collect_nodes(&golden.blocks, context)?;
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

fn collect_nodes(
    roots: &[BlockNode],
    context: &ExecutionContext,
) -> Result<Vec<SemanticNode>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(count_nodes(roots)).map_err(|_| working_overflow())?;
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
            kind: kind(&node.block),
            text: semantic_text(&node.block),
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

fn count_nodes(roots: &[BlockNode]) -> usize {
    fn count(nodes: &[BlockNode]) -> usize {
        nodes.iter().fold(0_usize, |total, node| {
            let nested = match &node.block {
                Block::List { items, .. } => items.iter().map(|item| count(&item.blocks)).sum(),
                Block::Table { rows, .. } => {
                    rows.iter().flat_map(|row| &row.cells).map(|cell| count(&cell.blocks)).sum()
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => count(blocks),
                _ => 0,
            };
            total.saturating_add(1).saturating_add(nested)
        })
    }
    count(roots)
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

fn semantic_text(block: &Block) -> String {
    match block {
        Block::Paragraph(values) | Block::Heading { content: values, .. } => inline_text(values),
        Block::Code { text, .. } | Block::Formula(text) => text.clone(),
        Block::Footnote { label, .. } => label.clone(),
        Block::Image { alt, .. } => alt.clone().unwrap_or_default(),
        Block::Page { number, .. } | Block::Slide { number, .. } => number.to_string(),
        Block::Sheet { name, .. } => name.clone(),
        Block::TimedSegment { content, .. } => inline_text(content),
        _ => String::new(),
    }
}

fn inline_text(values: &[Inline]) -> String {
    let mut output = String::new();
    let mut stack = vec![values.iter()];
    while let Some(iter) = stack.last_mut() {
        let Some(inline) = iter.next() else {
            stack.pop();
            continue;
        };
        match inline {
            Inline::Text { value, .. }
            | Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Code(value)
            | Inline::Formula(value)
            | Inline::FootnoteReference(value) => output.push_str(value),
            Inline::Link { content, .. } => stack.push(content.iter()),
            Inline::LineBreak => output.push('\n'),
        }
    }
    output
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
