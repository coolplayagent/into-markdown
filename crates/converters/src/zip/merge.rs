use super::allocation::{charged_format, reserve_append};
use into_markdown_core::{
    AssetId, Block, BlockNode, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    Document, ExecutionContext, Inline, NodeId, Provenance, ProvenanceKind, ResourceReservation,
    SourceLocator,
};
use std::collections::BTreeMap;

pub(super) struct MergeState<'a> {
    output: ConverterOutput,
    context: &'a ExecutionContext,
    memory: Option<ResourceReservation>,
    sequence: u64,
}

impl<'a> MergeState<'a> {
    pub(super) fn new(context: &'a ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            output: ConverterOutput::default(),
            context,
            memory: Some(context.reserve_memory(0)?),
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
        let mut scratch = self.context.reserve_memory(0)?;
        let scope = charged_format(&mut scratch, format_args!("zip-{}", self.sequence))?;
        reserve_append(
            &mut self.output.document.blocks,
            child.document.blocks.len().saturating_add(1),
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        reserve_append(
            &mut self.output.assets,
            child.assets.len(),
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        reserve_append(
            &mut self.output.diagnostics,
            child.diagnostics.len(),
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        self.output.absorb_memory_lease(&mut child, self.context)?;
        let mut asset_ids = BTreeMap::new();
        for asset in &mut child.assets {
            let old = std::mem::take(&mut asset.id.0);
            scratch.grow(128)?;
            let new = charged_format(&mut scratch, format_args!("{scope}-asset-{old}"))?;
            asset.id = AssetId(charged_format(
                self.memory.as_mut().ok_or_else(memory_unavailable)?,
                format_args!("{scope}-asset-{old}"),
            )?);
            asset_ids.insert(old, new);
            if let Some(filename) = &mut asset.filename {
                *filename = charged_format(
                    self.memory.as_mut().ok_or_else(memory_unavailable)?,
                    format_args!("{scope}-{filename}"),
                )?;
            }
        }

        let heading_id = charged_format(
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
            format_args!("{scope}-heading"),
        )?;
        let heading_text = charged_format(
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
            format_args!("{path}"),
        )?;
        let mut heading_content = Vec::new();
        reserve_append(
            &mut heading_content,
            1,
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        heading_content.push(Inline::Text { value: heading_text, marks: vec![] });
        let heading = BlockNode {
            id: NodeId(heading_id),
            block: Block::Heading { level: 2, content: heading_content },
            provenance: container_provenance(
                path,
                self.memory.as_mut().ok_or_else(memory_unavailable)?,
            )?,
        };
        self.output.document.blocks.push(heading);
        rewrite_nodes(&mut child.document.blocks, &scope, path, &asset_ids, self.memory()?)?;
        self.output.document.blocks.append(&mut child.document.blocks);
        self.output.assets.append(&mut child.assets);
        for diagnostic in &mut child.diagnostics {
            prefix_locator(diagnostic.locator.as_mut(), path, self.memory()?)?;
        }
        self.output.diagnostics.append(&mut child.diagnostics);
        merge_metadata(
            &mut self.output.document.metadata.properties,
            child.document.metadata,
            self.sequence,
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        Ok(())
    }

    pub(super) fn failure(
        &mut self,
        path: &str,
        error: &ConversionError,
    ) -> Result<(), ConversionError> {
        reserve_append(
            &mut self.output.diagnostics,
            1,
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        let diagnostic_code = if matches!(error, ConversionError::ArchiveExtractionRequired { .. })
        {
            "zip.entry.archiveExtractionRequired"
        } else {
            "zip.entry.failed"
        };
        let code = charged_format(self.memory()?, format_args!("{diagnostic_code}"))?;
        let message = charged_format(
            self.memory()?,
            format_args!("archive member {path:?} was skipped: {error}"),
        )?;
        let part = charged_format(self.memory()?, format_args!("{path}"))?;
        self.output.diagnostics.push(Diagnostic {
            code,
            severity: DiagnosticSeverity::Error,
            message,
            locator: Some(SourceLocator { part: Some(part), ..SourceLocator::default() }),
        });
        Ok(())
    }

    pub(super) fn resource_failure(
        &mut self,
        path: &str,
        error: &ConversionError,
        configured: Option<u64>,
    ) -> Result<(), ConversionError> {
        let Some((limit, _)) = error.limit() else {
            return self.failure(path, error);
        };
        reserve_append(
            &mut self.output.diagnostics,
            1,
            self.memory.as_mut().ok_or_else(memory_unavailable)?,
        )?;
        let code = charged_format(self.memory()?, format_args!("resource.{limit}.unitOmitted"))?;
        let configured = configured.map_or_else(|| "unknown".into(), |value| value.to_string());
        let message = charged_format(
            self.memory()?,
            format_args!(
                "resource limit {limit}: configured={configured}, observed=unknown, action=omitted 1 archiveMember; archive member {path:?} retained as a located omission ({error})"
            ),
        )?;
        let part = charged_format(self.memory()?, format_args!("{path}"))?;
        self.output.diagnostics.push(Diagnostic {
            code,
            severity: DiagnosticSeverity::Warning,
            message,
            locator: Some(SourceLocator { part: Some(part), ..SourceLocator::default() }),
        });
        Ok(())
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
        let memory = self.memory.take().ok_or_else(|| ConversionError::Internal {
            detail: "ZIP merge reservation was already transferred".into(),
        })?;
        self.output.attach_memory_reservation(self.context, memory)?;
        Ok(self.output)
    }

    fn memory(&mut self) -> Result<&mut ResourceReservation, ConversionError> {
        self.memory.as_mut().ok_or_else(|| ConversionError::Internal {
            detail: "ZIP merge reservation is unavailable".into(),
        })
    }
}

pub(super) fn rewrite_nodes(
    nodes: &mut [BlockNode],
    scope: &str,
    path: &str,
    asset_ids: &BTreeMap<String, String>,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    for node in nodes {
        let old = std::mem::take(&mut node.id.0);
        node.id.0 = charged_format(memory, format_args!("{scope}-node-{old}"))?;
        prefix_locator(Some(&mut node.provenance.locator), path, memory)?;
        match &mut node.block {
            Block::Paragraph(content)
            | Block::Heading { content, .. }
            | Block::TimedSegment { content, .. } => {
                rewrite_inlines(content, scope, path, memory)?;
            }
            Block::Image { asset, .. } => {
                let Some(replacement) = asset_ids.get(&asset.0) else {
                    return Err(ConversionError::Internal {
                        detail: charged_format(
                            memory,
                            format_args!("nested output references missing asset {}", asset.0),
                        )?,
                    });
                };
                asset.0 = charged_format(memory, format_args!("{replacement}"))?;
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
            Block::Footnote { label, blocks } => {
                *label = charged_format(memory, format_args!("{scope}-footnote-{label}"))?;
                rewrite_nodes(blocks, scope, path, asset_ids, memory)?;
            }
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                rewrite_nodes(blocks, scope, path, asset_ids, memory)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_inlines(
    inlines: &mut [Inline],
    scope: &str,
    path: &str,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    for inline in inlines {
        match inline {
            Inline::SourceText { provenance, .. } => {
                prefix_locator(Some(&mut provenance.locator), path, memory)?;
            }
            Inline::Link { content, .. } => rewrite_inlines(content, scope, path, memory)?,
            Inline::FootnoteReference(label) => {
                *label = charged_format(memory, format_args!("{scope}-footnote-{label}"))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn merge_metadata(
    target: &mut BTreeMap<String, String>,
    source: into_markdown_core::DocumentMetadata,
    sequence: u64,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    let prefix = charged_format(memory, format_args!("zip.entry.{sequence}"))?;
    if let Some(title) = source.title {
        insert_property(target, format_args!("{prefix}.title"), title, memory)?;
    }
    for (index, author) in source.authors.into_iter().enumerate() {
        insert_property(target, format_args!("{prefix}.author.{index}"), author, memory)?;
    }
    for (key, value) in source.properties {
        insert_property(target, format_args!("{prefix}.property.{key}"), value, memory)?;
    }
    Ok(())
}

fn insert_property(
    target: &mut BTreeMap<String, String>,
    key: std::fmt::Arguments<'_>,
    value: String,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    memory.grow(128)?;
    let key = charged_format(memory, key)?;
    if target.insert(key, value).is_some() {
        return Err(ConversionError::Internal { detail: "ZIP metadata namespace collided".into() });
    }
    Ok(())
}

fn prefix_locator(
    locator: Option<&mut SourceLocator>,
    path: &str,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    let Some(locator) = locator else { return Ok(()) };
    locator.part = Some(match locator.part.take() {
        Some(child) => charged_format(memory, format_args!("{path}/{child}"))?,
        None => charged_format(memory, format_args!("{path}"))?,
    });
    Ok(())
}

fn container_provenance(
    path: &str,
    memory: &mut ResourceReservation,
) -> Result<Provenance, ConversionError> {
    Ok(Provenance {
        kind: ProvenanceKind::Metadata,
        provider: charged_format(memory, format_args!("builtin.converter.zip"))?,
        locator: SourceLocator {
            part: Some(charged_format(memory, format_args!("{path}"))?),
            ..SourceLocator::default()
        },
        confidence: Some(1.0),
    })
}

fn memory_unavailable() -> ConversionError {
    ConversionError::Internal { detail: "ZIP merge reservation is unavailable".into() }
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "ZIP merge memory size overflowed".into(),
    }
}
