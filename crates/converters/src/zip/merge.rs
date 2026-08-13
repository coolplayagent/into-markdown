use into_markdown_core::{
    AssetId, Block, BlockNode, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    Document, ExecutionContext, Inline, NodeId, Provenance, ProvenanceKind, ResourceReservation,
    SourceLocator,
};
use std::collections::BTreeMap;

pub(super) struct MergeState<'a> {
    output: ConverterOutput,
    context: &'a ExecutionContext,
    memory: ResourceReservation,
    sequence: u64,
}

impl<'a> MergeState<'a> {
    pub(super) fn new(context: &'a ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            output: ConverterOutput::default(),
            context,
            memory: context.reserve_memory(0)?,
            sequence: 0,
        })
    }

    pub(super) fn append(
        &mut self,
        path: &str,
        mut child: ConverterOutput,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.sequence = self.sequence.checked_add(1).ok_or_else(memory_overflow)?;
        let scope = format!("zip-{}", self.sequence);
        let mut asset_ids = BTreeMap::new();
        for asset in &mut child.assets {
            let old = asset.id.0.clone();
            let new = format!("{scope}-asset-{old}");
            asset_ids.insert(old, new.clone());
            asset.id = AssetId(new);
            if let Some(filename) = &mut asset.filename {
                *filename = format!("{scope}-{filename}");
            }
        }
        let asset_bytes = child.assets.iter().try_fold(0_u64, |total, asset| {
            total
                .checked_add(u64::try_from(asset.bytes.capacity()).unwrap_or(u64::MAX))
                .ok_or_else(memory_overflow)
        })?;
        self.memory.grow(asset_bytes)?;

        let heading = BlockNode {
            id: NodeId(format!("{scope}-heading")),
            block: Block::Heading {
                level: 2,
                content: vec![Inline::Text { value: path.to_owned(), marks: vec![] }],
            },
            provenance: container_provenance(path),
        };
        self.charge_text(path)?;
        self.output.document.blocks.push(heading);
        rewrite_nodes(&mut child.document.blocks, &scope, path, &asset_ids, &mut self.memory)?;
        self.output.document.blocks.append(&mut child.document.blocks);
        self.output.assets.append(&mut child.assets);
        for diagnostic in &mut child.diagnostics {
            prefix_locator(diagnostic.locator.as_mut(), path);
        }
        self.output.diagnostics.append(&mut child.diagnostics);
        if let Some(title) = child.document.metadata.title {
            self.output
                .document
                .metadata
                .properties
                .insert(format!("zip.entry.{}.title", self.sequence), title);
        }
        Ok(())
    }

    pub(super) fn failure(&mut self, path: &str, error: &ConversionError) {
        self.output.diagnostics.push(Diagnostic {
            code: "zip.entry.failed".into(),
            severity: DiagnosticSeverity::Error,
            message: format!("archive member {path:?} was skipped: {error}"),
            locator: Some(SourceLocator { part: Some(path.into()), ..SourceLocator::default() }),
        });
    }

    pub(super) fn finish(mut self) -> Result<ConverterOutput, ConversionError> {
        if self.output.document.blocks.is_empty() {
            self.output.document = Document::default();
        }
        self.output.document.validate().map_err(|error| {
            if error.code == into_markdown_core::IrErrorCode::ResourceLimit {
                ConversionError::ResourceLimit {
                    limit: "document_structure",
                    detail: format!("merged ZIP output exceeds IR limits at {}", error.path),
                }
            } else {
                ConversionError::Internal {
                    detail: format!(
                        "merged ZIP output is invalid at {}: {}",
                        error.path, error.detail
                    ),
                }
            }
        })?;
        Ok(self.output)
    }

    fn charge_text(&mut self, value: &str) -> Result<(), ConversionError> {
        self.memory.grow(u64::try_from(value.len()).unwrap_or(u64::MAX))
    }
}

fn rewrite_nodes(
    nodes: &mut [BlockNode],
    scope: &str,
    path: &str,
    asset_ids: &BTreeMap<String, String>,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    for node in nodes {
        let old = std::mem::take(&mut node.id.0);
        node.id.0 = format!("{scope}-node-{old}");
        memory.grow(u64::try_from(node.id.0.capacity()).unwrap_or(u64::MAX))?;
        prefix_locator(Some(&mut node.provenance.locator), path);
        match &mut node.block {
            Block::Image { asset, .. } => {
                let replacement =
                    asset_ids.get(&asset.0).ok_or_else(|| ConversionError::Internal {
                        detail: format!("nested output references missing asset {}", asset.0),
                    })?;
                asset.0.clone_from(replacement);
            }
            Block::List { items, .. } => {
                for item in items {
                    rewrite_nodes(&mut item.blocks, scope, path, asset_ids, memory)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        rewrite_nodes(&mut cell.blocks, scope, path, asset_ids, memory)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                rewrite_nodes(blocks, scope, path, asset_ids, memory)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn prefix_locator(locator: Option<&mut SourceLocator>, path: &str) {
    let Some(locator) = locator else { return };
    locator.part = Some(match locator.part.take() {
        Some(child) => format!("{path}/{child}"),
        None => path.to_owned(),
    });
}

fn container_provenance(path: &str) -> Provenance {
    Provenance {
        kind: ProvenanceKind::Metadata,
        provider: "builtin.converter.zip".into(),
        locator: SourceLocator { part: Some(path.into()), ..SourceLocator::default() },
        confidence: Some(1.0),
    }
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "ZIP merge memory size overflowed".into(),
    }
}
