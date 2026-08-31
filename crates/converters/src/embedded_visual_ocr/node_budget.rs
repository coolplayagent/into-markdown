//! Count actual OCR nodes before caching or copying them into the native document.

use super::{VisualRef, count_document_nodes, resource};
use into_markdown_core::{
    AssetId, BlockNode, ConversionError, ExecutionContext, MAX_DOCUMENT_NODES,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn reference_counts(
    references: &[VisualRef],
    assets: &BTreeMap<AssetId, usize>,
    eligible: &BTreeSet<AssetId>,
    maximum: u32,
    context: &ExecutionContext,
) -> Result<BTreeMap<AssetId, u64>, ConversionError> {
    let mut counts = BTreeMap::<AssetId, u64>::new();
    let mut total = 0_u32;
    for reference in references {
        context.checkpoint()?;
        if !assets.contains_key(&reference.asset) {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", reference.asset.0),
            });
        }
        if !eligible.contains(&reference.asset) {
            continue;
        }
        total = total.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "embedded visual reference count overflow")
        })?;
        if total > maximum {
            return Err(resource(
                "max_archive_entries",
                "embedded visual references exceed the request limit",
            ));
        }
        *counts.entry(reference.asset.clone()).or_default() += 1;
    }
    Ok(counts)
}

pub(super) struct NodeBudget {
    nodes: usize,
}

impl NodeBudget {
    pub(super) fn new(nodes: usize) -> Result<Self, ConversionError> {
        if nodes > MAX_DOCUMENT_NODES {
            return Err(resource("documentNodes", "native document exceeds the node limit"));
        }
        Ok(Self { nodes })
    }

    pub(super) fn admit(
        &mut self,
        contribution: &[BlockNode],
        reference_copies: u64,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let count = count_document_nodes(contribution, context)?;
        let copies = usize::try_from(reference_copies)
            .map_err(|_| resource("documentNodes", "OCR reference count overflow"))?;
        let next = count
            .checked_mul(copies)
            .and_then(|added| self.nodes.checked_add(added))
            .ok_or_else(|| resource("documentNodes", "OCR output node count overflow"))?;
        if next > MAX_DOCUMENT_NODES {
            return Err(resource(
                "documentNodes",
                format!(
                    "native and recognized output requires {next} nodes; limit is {MAX_DOCUMENT_NODES}"
                ),
            ));
        }
        self.nodes = next;
        Ok(())
    }
}
