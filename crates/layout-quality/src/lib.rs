//! Deterministic cross-format semantic layout quality gate.
//!
//! Converters remain authoritative for format parsing and emit the common
//! document IR. This crate projects that IR through a deliberately narrow,
//! offline interface and compares reading order, hierarchy, table topology,
//! source boundaries, provenance, and resource associations. Renderers are
//! observed through a hash-pinned GFM input; this layer never repairs output.

mod geometry;
mod model;
mod paragraph_list;
mod quality_metrics;
mod reading_order;
mod resource_association;
mod table;

pub use model::*;

use into_markdown_core::{
    Asset, Block, BlockNode, ConversionError, Document, ExecutionContext, Inline,
};
use sha2::{Digest as _, Sha256};
use std::io::{self, Write};

/// Build the deterministic semantic projection without comparing a golden.
///
/// This is intended for reviewed authority generation and diagnostics. Normal
/// quality gates should call [`audit`] so the projection cannot be mistaken for
/// an accepted baseline.
///
/// # Errors
///
/// Returns typed cancellation, timeout, malformed-geometry, depth, table, work,
/// or memory-limit errors from the shared request context.
pub fn project(
    document: &Document,
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<SemanticProjection, ConversionError> {
    context.checkpoint()?;
    let retained = estimate_projection_memory(document, assets, context)?;
    let memory_lease = context.reserve_memory(retained)?;
    let snapshot = reading_order::snapshot(document, assets, context)?;
    context.checkpoint()?;
    Ok(SemanticProjection { snapshot, memory_lease })
}

/// Audit one conversion result against a reviewed, hash-pinned authority.
///
/// The function is transactional: every fallible check completes before a
/// report is returned. The successful report owns the request-memory lease for
/// its retained strings and diffs; every error path drops that lease.
///
/// # Errors
///
/// Returns typed cancellation, timeout, malformed authority/IR, depth, work,
/// table, or memory-limit errors. No partial report is returned.
pub fn audit(
    authority: &FixtureAuthority,
    document: &Document,
    assets: &[Asset],
    gfm: &str,
    context: &ExecutionContext,
) -> Result<QualityReport, ConversionError> {
    validate_authority(authority)?;
    context.checkpoint()?;
    let retained = estimate_report_memory(document, assets, authority, context)?;
    let memory_lease = context.reserve_memory(retained)?;
    let actual = reading_order::snapshot(document, assets, context)?;
    context.checkpoint()?;
    let ir_sha256 = hash_ir(document, context)?;
    let gfm_sha256 = hash_bytes(gfm.as_bytes(), context)?;
    let (metrics, mut diffs) = quality_metrics::compare(authority, &actual, context)?;
    if ir_sha256 != authority.ir_sha256 {
        diffs.push(hash_diff(DiffKind::IrGolden, authority, &authority.ir_sha256, &ir_sha256));
    }
    if gfm_sha256 != authority.gfm_sha256 {
        diffs.push(hash_diff(DiffKind::GfmGolden, authority, &authority.gfm_sha256, &gfm_sha256));
    }
    let threshold = QualityThreshold::for_cohort(authority.cohort);
    if metrics.precision_basis_points < threshold.minimum_precision_basis_points
        || metrics.recall_basis_points < threshold.minimum_recall_basis_points
    {
        diffs.push(QualityDiff {
            kind: DiffKind::Threshold,
            fixture_id: authority.fixture_id.clone(),
            node_id: None,
            location: "document".into(),
            expected: Some(format!(
                "precision>={},recall>={}",
                threshold.minimum_precision_basis_points, threshold.minimum_recall_basis_points
            )),
            actual: Some(format!(
                "precision={},recall={}",
                metrics.precision_basis_points, metrics.recall_basis_points
            )),
        });
    }
    diffs.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.location.cmp(&right.location))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    context.checkpoint()?;
    Ok(QualityReport {
        fixture_id: authority.fixture_id.clone(),
        passed: diffs.is_empty(),
        metrics,
        ir_sha256,
        gfm_sha256,
        diffs,
        memory_lease,
    })
}

fn validate_authority(authority: &FixtureAuthority) -> Result<(), ConversionError> {
    if authority.schema_version != AUTHORITY_SCHEMA_VERSION {
        return Err(ConversionError::Malformed {
            part: Some("semantic-layout-quality-authority".into()),
            detail: format!(
                "unsupported schema version {}; expected {AUTHORITY_SCHEMA_VERSION}",
                authority.schema_version
            ),
        });
    }
    if authority.fixture_id.trim().is_empty() || authority.format.trim().is_empty() {
        return Err(ConversionError::Malformed {
            part: Some("semantic-layout-quality-authority".into()),
            detail: "fixture ID and format must be non-empty".into(),
        });
    }
    if !is_sha256(&authority.ir_sha256) || !is_sha256(&authority.gfm_sha256) {
        return Err(ConversionError::Malformed {
            part: Some("semantic-layout-quality-authority".into()),
            detail: "IR and GFM authorities must be lowercase SHA-256 digests".into(),
        });
    }
    Ok(())
}

fn estimate_report_memory(
    document: &Document,
    assets: &[Asset],
    authority: &FixtureAuthority,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let (mut estimate, mut work) = estimate_projection(document, assets, context)?;
    let expected_nodes = u64::try_from(authority.snapshot.nodes.len())
        .map_err(|_| work_limit("authority node count"))?;
    let expected_assets = u64::try_from(authority.snapshot.assets.len())
        .map_err(|_| work_limit("authority asset count"))?;
    spend_work(
        &mut work,
        expected_nodes.saturating_add(expected_assets),
        context,
        "authority comparison work",
    )?;
    estimate = checked_add(
        estimate,
        expected_nodes.saturating_mul(16 * 1024),
        "difference report memory",
    )?;
    Ok(estimate)
}

fn estimate_projection_memory(
    document: &Document,
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    estimate_projection(document, assets, context).map(|(bytes, _)| bytes)
}

fn estimate_projection(
    document: &Document,
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<(u64, u64), ConversionError> {
    let mut estimate = 16 * 1024_u64;
    let mut work = 0_u64;
    for block in &document.blocks {
        estimate_node(block, 0, &mut work, &mut estimate, context)?;
    }
    for asset in assets {
        context.checkpoint()?;
        spend_work(&mut work, 1, context, "asset work")?;
        estimate = checked_add(
            estimate,
            1024_u64
                .saturating_add(string_bytes(&asset.id.0))
                .saturating_add(string_bytes(&asset.media_type))
                .saturating_add(asset.filename.as_deref().map_or(0, string_bytes))
                .saturating_add(asset.external_uri.as_deref().map_or(0, string_bytes)),
            "asset report memory",
        )?;
    }
    Ok((estimate, work))
}

fn estimate_node(
    node: &BlockNode,
    depth: u16,
    work: &mut u64,
    estimate: &mut u64,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    if depth > context.resource_limits().max_nesting_depth {
        return Err(ConversionError::ResourceLimit {
            limit: "max_nesting_depth",
            detail: format!(
                "semantic layout depth {depth} > {}",
                context.resource_limits().max_nesting_depth
            ),
        });
    }
    spend_work(work, 1, context, "node work")?;
    *estimate = checked_add(
        *estimate,
        4096_u64
            .saturating_add(string_bytes(&node.id.0))
            .saturating_add(string_bytes(&node.provenance.provider))
            .saturating_add(node.provenance.locator.part.as_deref().map_or(0, string_bytes))
            .saturating_add(node.provenance.locator.sheet.as_deref().map_or(0, string_bytes))
            .saturating_add(block_dynamic_bytes(&node.block, context, work)?),
        "node report memory",
    )?;
    let child_depth = depth.checked_add(1).ok_or_else(|| work_limit("nesting depth"))?;
    match &node.block {
        Block::List { items, .. } => {
            for item in items {
                for child in &item.blocks {
                    estimate_node(child, child_depth, work, estimate, context)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &row.cells {
                    for child in &cell.blocks {
                        estimate_node(child, child_depth, work, estimate, context)?;
                    }
                }
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            for child in blocks {
                estimate_node(child, child_depth, work, estimate, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn block_dynamic_bytes(
    block: &Block,
    context: &ExecutionContext,
    work: &mut u64,
) -> Result<u64, ConversionError> {
    Ok(match block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            inline_dynamic_bytes(inlines, context, work)?
        }
        Block::Code { text, language } => {
            string_bytes(text).saturating_add(language.as_deref().map_or(0, string_bytes))
        }
        Block::Formula(value) => string_bytes(value),
        Block::Footnote { label, .. } => string_bytes(label),
        Block::Image { asset, alt } => {
            string_bytes(&asset.0).saturating_add(alt.as_deref().map_or(0, string_bytes))
        }
        Block::Slide { title, .. } => title.as_deref().map_or(0, string_bytes),
        Block::Sheet { name, .. } => string_bytes(name),
        Block::List { items, .. } => {
            let mut bytes = 0_u64;
            for item in items {
                context.checkpoint()?;
                spend_work(work, 1, context, "list item work")?;
                bytes = bytes
                    .saturating_add(256)
                    .saturating_add(item.marker_label.as_deref().map_or(0, string_bytes));
            }
            bytes
        }
        Block::Table { rows, .. } => {
            let mut bytes = 0_u64;
            for row in rows {
                context.checkpoint()?;
                for cell in &row.cells {
                    context.checkpoint()?;
                    let occupied_cells = u64::from(cell.row_span)
                        .checked_mul(u64::from(cell.column_span))
                        .ok_or_else(|| work_limit("table span work"))?;
                    spend_work(work, occupied_cells, context, "table cell work")?;
                    let occupied = occupied_cells.saturating_mul(128);
                    bytes = bytes.saturating_add(512).saturating_add(occupied);
                    for child in &cell.blocks {
                        bytes = bytes.saturating_add(string_bytes(&child.id.0));
                    }
                }
            }
            bytes
        }
        _ => 0,
    })
}

fn inline_dynamic_bytes(
    inlines: &[Inline],
    context: &ExecutionContext,
    work: &mut u64,
) -> Result<u64, ConversionError> {
    let mut bytes = 0_u64;
    for inline in inlines {
        context.checkpoint()?;
        spend_work(work, 1, context, "inline work")?;
        bytes = bytes.saturating_add(match inline {
            Inline::Text { value, marks } | Inline::SourceText { value, marks, .. } => {
                string_bytes(value).saturating_add(
                    u64::try_from(marks.len()).unwrap_or(u64::MAX).saturating_mul(16),
                )
            }
            Inline::OcrText { value, marks, evidence, .. } => {
                let regions = u64::try_from(evidence.regions.len()).unwrap_or(u64::MAX);
                let chain = u64::try_from(evidence.chain.len()).unwrap_or(u64::MAX);
                spend_work(work, regions.saturating_add(chain), context, "OCR evidence work")?;
                string_bytes(value)
                    .saturating_add(
                        u64::try_from(marks.len()).unwrap_or(u64::MAX).saturating_mul(16),
                    )
                    .saturating_add(regions.saturating_mul(256))
                    .saturating_add(evidence.chain.iter().fold(0_u64, |sum, step| {
                        sum.saturating_add(128)
                            .saturating_add(string_bytes(&step.provider))
                            .saturating_add(step.model.as_deref().map_or(0, string_bytes))
                    }))
            }
            Inline::Code(value) | Inline::Formula(value) | Inline::FootnoteReference(value) => {
                string_bytes(value)
            }
            Inline::Link { target, content } => {
                string_bytes(target).saturating_add(inline_dynamic_bytes(content, context, work)?)
            }
            Inline::LineBreak => 1,
            _ => 128,
        });
    }
    Ok(bytes)
}

fn hash_ir(document: &Document, context: &ExecutionContext) -> Result<String, ConversionError> {
    let mut writer = DigestWriter { digest: Sha256::new(), context, checkpoint_error: None };
    if let Err(error) = serde_json::to_writer(&mut writer, document) {
        return Err(writer.checkpoint_error.take().unwrap_or_else(|| ConversionError::Internal {
            detail: format!("serialize semantic layout IR golden: {error}"),
        }));
    }
    Ok(hex(writer.digest.finalize()))
}

fn hash_bytes(bytes: &[u8], context: &ExecutionContext) -> Result<String, ConversionError> {
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        context.checkpoint()?;
        digest.update(chunk);
    }
    Ok(hex(digest.finalize()))
}

struct DigestWriter<'a> {
    digest: Sha256,
    context: &'a ExecutionContext,
    checkpoint_error: Option<ConversionError>,
}

impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Err(error) = self.context.checkpoint() {
            self.checkpoint_error = Some(error);
            return Err(io::Error::other("semantic layout hash interrupted"));
        }
        self.digest.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hash_diff(
    kind: DiffKind,
    authority: &FixtureAuthority,
    expected: &str,
    actual: &str,
) -> QualityDiff {
    QualityDiff {
        kind,
        fixture_id: authority.fixture_id.clone(),
        node_id: None,
        location: "document".into(),
        expected: Some(expected.into()),
        actual: Some(actual.into()),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn string_bytes(value: &str) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

fn checked_add(left: u64, right: u64, detail: &'static str) -> Result<u64, ConversionError> {
    left.checked_add(right).ok_or_else(|| work_limit(detail))
}

fn spend_work(
    work: &mut u64,
    units: u64,
    context: &ExecutionContext,
    detail: &'static str,
) -> Result<(), ConversionError> {
    *work = checked_add(*work, units, detail)?;
    let limit = context.resource_limits().max_table_cells;
    if *work > limit {
        return Err(ConversionError::ResourceLimit {
            limit: "semantic_layout_work",
            detail: format!("{work} > {limit} work units ({detail})"),
        });
    }
    Ok(())
}

fn work_limit(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "semantic_layout_work", detail: detail.into() }
}

#[cfg(test)]
mod tests;
