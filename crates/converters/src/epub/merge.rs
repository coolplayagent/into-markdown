//! EPUB-specific IR assembly and identity-domain rewriting.

use super::encryption::EncryptionPolicy;
use super::navigation::{NavEntry, Navigation};
use super::package::Package;
use super::resources::{CoverResource, ResourceStore};
use super::spine::SpineResult;
use into_markdown_core::{
    AssetId, Block, BlockNode, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    Document, ExecutionContext, Inline, ListItem, ListKind, MAX_DOCUMENT_DEPTH, NodeId, Provenance,
    ProvenanceKind, SourceLocator,
};
use std::collections::{BTreeMap, BTreeSet};

const PROVIDER: &str = "builtin.converter.epub";

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
// Assembly owns the cross-module boundary values so all identity rewrites are transactional.
pub(super) fn assemble(
    mut package: Package,
    navigation: Option<Navigation>,
    mut spine: SpineResult,
    resources: ResourceStore,
    cover: Option<CoverResource>,
    encryption: EncryptionPolicy,
    rights_metadata: bool,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let anchors = spine
        .chapters
        .iter()
        .flat_map(|chapter| chapter.anchors.iter().cloned())
        .collect::<BTreeSet<_>>();
    let chapter_paths =
        spine.chapters.iter().map(|chapter| chapter.path.clone()).collect::<BTreeSet<_>>();
    let mut footnote_labels = BTreeMap::new();
    for chapter in &spine.chapters {
        for footnote in &chapter.footnotes {
            let next = format!("epub-footnote-{:06}", footnote_labels.len() + 1);
            if footnote_labels.insert(footnote.target.clone(), next).is_some() {
                return Err(malformed("duplicate EPUB footnote target"));
            }
        }
    }
    validate_targets(&spine, navigation.as_ref(), &anchors, &chapter_paths, &footnote_labels)?;

    let metadata = std::mem::take(&mut package.metadata);
    let mut output =
        ConverterOutput::new(Document { metadata, ..Document::default() }, Vec::new(), Vec::new());
    if let Some(navigation) = navigation {
        append_navigation(&mut output.document.blocks, navigation)?;
    }
    if let Some(cover) = cover {
        output.document.blocks.push(node(
            "epub-cover-heading".into(),
            Block::Heading {
                level: 1,
                content: vec![Inline::Text { value: "Cover".into(), marks: vec![] }],
            },
            &cover.path,
            ProvenanceKind::Metadata,
        ));
        output.document.blocks.push(node(
            "epub-cover-image".into(),
            Block::Image { asset: cover.id, alt: Some("Cover".into()) },
            &cover.path,
            ProvenanceKind::Metadata,
        ));
    }
    if spine.skipped_non_linear > 0 {
        output.diagnostics.push(diagnostic(
            "epub.spine.nonLinearSkipped",
            DiagnosticSeverity::Info,
            format!("skipped {} non-linear spine item(s)", spine.skipped_non_linear),
            Some(&package.path),
        ));
    }
    if rights_metadata {
        output.diagnostics.push(diagnostic(
            "epub.rightsMetadataIgnored",
            DiagnosticSeverity::Info,
            "META-INF/rights.xml was retained as inert metadata and not interpreted".into(),
            Some("META-INF/rights.xml"),
        ));
    }
    for path in encryption.unavailable_fonts {
        output.diagnostics.push(diagnostic(
            "epub.fontObfuscationUnsupported",
            DiagnosticSeverity::Warning,
            "an IDPF/Adobe-obfuscated font was unavailable and was not decoded".into(),
            Some(&path),
        ));
    }
    append_omitted_resource_diagnostic(&mut output, &package);

    for (index, chapter) in spine.chapters.iter_mut().enumerate() {
        let sequence = index + 1;
        context.checkpoint()?;
        output.absorb_memory_lease(&mut chapter.output, context)?;
        let mut asset_ids = BTreeMap::new();
        for asset in &mut chapter.output.assets {
            let old = std::mem::take(&mut asset.id.0);
            let new = format!("epub-spine-{sequence:06}-asset-{old}");
            asset.id = AssetId(new.clone());
            asset_ids.insert(old, AssetId(new));
            if let Some(filename) = &mut asset.filename {
                *filename = format!("epub-spine-{sequence:06}-{filename}");
            }
        }
        rewrite_nodes(
            &mut chapter.output.document.blocks,
            sequence,
            &chapter.path,
            &asset_ids,
            &chapter.references,
            &footnote_labels,
        )?;
        let title = chapter_title(chapter, &package, sequence);
        output.document.blocks.push(node(
            format!("epub-spine-{sequence:06}-heading"),
            Block::Heading {
                level: 1,
                content: vec![Inline::Text { value: title, marks: vec![] }],
            },
            &chapter.path,
            ProvenanceKind::Metadata,
        ));
        output.document.blocks.append(&mut chapter.output.document.blocks);
        for footnote in &chapter.footnotes {
            let label = footnote_labels.get(&footnote.target).ok_or_else(|| {
                ConversionError::Internal { detail: "EPUB footnote label inventory changed".into() }
            })?;
            output.document.blocks.push(node(
                format!("epub-spine-{sequence:06}-footnote-{label}"),
                Block::Footnote {
                    label: label.clone(),
                    blocks: vec![node(
                        format!("epub-spine-{sequence:06}-footnote-{label}-text"),
                        Block::Paragraph(vec![Inline::Text {
                            value: footnote.text.clone(),
                            marks: vec![],
                        }]),
                        &chapter.path,
                        ProvenanceKind::NativeParser,
                    )],
                },
                &chapter.path,
                ProvenanceKind::NativeParser,
            ));
        }
        output.assets.append(&mut chapter.output.assets);
        for diagnostic in &mut chapter.output.diagnostics {
            prefix_locator(diagnostic.locator.as_mut(), &chapter.path);
        }
        output.diagnostics.append(&mut chapter.output.diagnostics);
        if let Some(title) = chapter.output.document.metadata.title.take() {
            output
                .document
                .metadata
                .properties
                .insert(format!("epub.spine.{sequence}.title"), title);
        }
    }
    resources.finish(&mut output, context)?;
    output.document.validate().map_err(|error| {
        if error.code == into_markdown_core::IrErrorCode::ResourceLimit {
            ConversionError::ResourceLimit {
                limit: "document_structure",
                detail: format!("assembled EPUB output exceeds IR limits at {}", error.path),
            }
        } else {
            ConversionError::Internal {
                detail: format!("assembled EPUB IR is invalid at {}: {}", error.path, error.detail),
            }
        }
    })?;
    output.account_retained(context)
}

fn append_navigation(
    blocks: &mut Vec<BlockNode>,
    navigation: Navigation,
) -> Result<(), ConversionError> {
    let Navigation { source_path, entries } = navigation;
    if entries.iter().any(|entry| entry.depth.saturating_add(2) > MAX_DOCUMENT_DEPTH) {
        return Err(ConversionError::ResourceLimit {
            limit: "documentDepth",
            detail: format!(
                "EPUB navigation exceeds the unified IR depth limit {MAX_DOCUMENT_DEPTH}"
            ),
        });
    }
    blocks.push(node(
        "epub-navigation-heading".into(),
        Block::Heading {
            level: 1,
            content: vec![Inline::Text { value: "Contents".into(), marks: vec![] }],
        },
        &source_path,
        ProvenanceKind::Metadata,
    ));
    let mut cursor = 0;
    let mut list_sequence = 1;
    let items = navigation_items(&entries, &mut cursor, 0, &mut list_sequence, &source_path)?;
    if items.is_empty() {
        return Err(malformed("navigation has no entries"));
    }
    if cursor != entries.len() {
        return Err(malformed("navigation hierarchy is incomplete"));
    }
    blocks.push(node(
        "epub-navigation-list".into(),
        Block::List { kind: ListKind::Ordered, start: 1, items },
        &source_path,
        ProvenanceKind::NativeParser,
    ));
    Ok(())
}

fn navigation_items(
    entries: &[NavEntry],
    cursor: &mut usize,
    depth: usize,
    list_sequence: &mut usize,
    source_path: &str,
) -> Result<Vec<ListItem>, ConversionError> {
    let mut items = Vec::<ListItem>::new();
    while let Some(entry) = entries.get(*cursor) {
        if entry.depth < depth {
            break;
        }
        if entry.depth > depth {
            if entry.depth != depth + 1 || items.is_empty() {
                return Err(malformed("navigation hierarchy skips a parent level"));
            }
            let nested = navigation_items(entries, cursor, depth + 1, list_sequence, source_path)?;
            if nested.is_empty() {
                return Err(malformed("navigation contains an empty nested level"));
            }
            let sequence = *list_sequence;
            *list_sequence = list_sequence.saturating_add(1);
            items
                .last_mut()
                .ok_or_else(|| malformed("navigation child has no parent"))?
                .blocks
                .push(node(
                    format!("epub-navigation-list-{sequence:06}"),
                    Block::List { kind: ListKind::Ordered, start: 1, items: nested },
                    source_path,
                    ProvenanceKind::NativeParser,
                ));
            continue;
        }
        let sequence = *cursor;
        items.push(ListItem {
            checked: None,
            marker_label: None,
            blocks: vec![node(
                format!("epub-navigation-entry-{sequence:06}"),
                Block::Paragraph(match &entry.target {
                    Some(target) => vec![Inline::Link {
                        target: target.clone(),
                        content: vec![Inline::Text { value: entry.label.clone(), marks: vec![] }],
                    }],
                    None => vec![Inline::Text { value: entry.label.clone(), marks: vec![] }],
                }),
                source_path,
                ProvenanceKind::NativeParser,
            )],
        });
        *cursor = cursor.saturating_add(1);
    }
    Ok(items)
}

fn rewrite_nodes(
    nodes: &mut [BlockNode],
    sequence: usize,
    path: &str,
    asset_ids: &BTreeMap<String, AssetId>,
    references: &BTreeMap<String, String>,
    footnotes: &BTreeMap<String, String>,
) -> Result<(), ConversionError> {
    for node in nodes {
        node.id.0 = format!("epub-spine-{sequence:06}-node-{}", node.id.0);
        prefix_locator(Some(&mut node.provenance.locator), path);
        match &mut node.block {
            Block::Paragraph(content)
            | Block::Heading { content, .. }
            | Block::TimedSegment { content, .. } => {
                rewrite_inlines(content, sequence, path, references, footnotes)?;
            }
            Block::Image { asset, .. } => {
                if let Some(replacement) = asset_ids.get(&asset.0) {
                    *asset = replacement.clone();
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    rewrite_nodes(
                        &mut item.blocks,
                        sequence,
                        path,
                        asset_ids,
                        references,
                        footnotes,
                    )?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        rewrite_nodes(
                            &mut cell.blocks,
                            sequence,
                            path,
                            asset_ids,
                            references,
                            footnotes,
                        )?;
                    }
                }
            }
            Block::Footnote { label, blocks } => {
                *label = format!("epub-spine-{sequence:06}-{label}");
                rewrite_nodes(blocks, sequence, path, asset_ids, references, footnotes)?;
            }
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                rewrite_nodes(blocks, sequence, path, asset_ids, references, footnotes)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_inlines(
    inlines: &mut [Inline],
    sequence: usize,
    path: &str,
    references: &BTreeMap<String, String>,
    footnotes: &BTreeMap<String, String>,
) -> Result<(), ConversionError> {
    for inline in inlines {
        match inline {
            Inline::SourceText { provenance, .. } => {
                prefix_locator(Some(&mut provenance.locator), path);
            }
            Inline::Link { target, content } => {
                rewrite_inlines(content, sequence, path, references, footnotes)?;
                if let Some(canonical) = references.get(target) {
                    *target = canonical.clone();
                }
                if let Some(label) = footnotes.get(target) {
                    *inline = Inline::FootnoteReference(label.clone());
                }
            }
            Inline::FootnoteReference(label) => {
                *label = format!("epub-spine-{sequence:06}-{label}");
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_targets(
    spine: &SpineResult,
    navigation: Option<&Navigation>,
    anchors: &BTreeSet<String>,
    chapter_paths: &BTreeSet<String>,
    footnotes: &BTreeMap<String, String>,
) -> Result<(), ConversionError> {
    let targets = spine.chapters.iter().flat_map(|chapter| chapter.internal_targets.iter()).chain(
        navigation
            .into_iter()
            .flat_map(|nav| nav.entries.iter().filter_map(|entry| entry.target.as_ref())),
    );
    for target in targets {
        if let Some((path, _)) = target.split_once('#')
            && chapter_paths.contains(path)
            && !anchors.contains(target)
            && !footnotes.contains_key(target)
        {
            return Err(ConversionError::Malformed {
                part: Some(path.into()),
                detail: format!("EPUB link target {target:?} names a missing fragment"),
            });
        }
    }
    Ok(())
}

fn chapter_title(chapter: &super::spine::Chapter, package: &Package, sequence: usize) -> String {
    chapter
        .output
        .document
        .metadata
        .title
        .clone()
        .or_else(|| {
            package.metadata.properties.get(&format!("epub.spine.{sequence}.title")).cloned()
        })
        .unwrap_or_else(|| chapter.path.rsplit('/').next().unwrap_or(&chapter.path).to_owned())
}

fn append_omitted_resource_diagnostic(output: &mut ConverterOutput, package: &Package) {
    let mut css = 0;
    let mut fonts = 0;
    let mut media = 0;
    for item in package.manifest.values() {
        match item.media_type.as_str() {
            "text/css" => css += 1,
            value if value.starts_with("font/") || value.contains("font") => fonts += 1,
            value if value.starts_with("audio/") || value.starts_with("video/") => media += 1,
            _ => {}
        }
    }
    if css + fonts + media > 0 {
        output.diagnostics.push(diagnostic(
            "epub.unreferencedResourcesOmitted",
            DiagnosticSeverity::Info,
            format!(
                "omitted {css} CSS, {fonts} font, and {media} audio/video manifest resource(s) without an IR reference contract"
            ),
            Some(&package.path),
        ));
    }
}

fn node(id: String, block: Block, part: &str, kind: ProvenanceKind) -> BlockNode {
    BlockNode {
        id: NodeId(id),
        block,
        provenance: Provenance {
            kind,
            provider: PROVIDER.into(),
            locator: SourceLocator { part: Some(part.into()), ..SourceLocator::default() },
            confidence: Some(1.0),
        },
    }
}

fn diagnostic(
    code: &str,
    severity: DiagnosticSeverity,
    message: String,
    part: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        severity,
        message,
        locator: part
            .map(|part| SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
    }
}

fn prefix_locator(locator: Option<&mut SourceLocator>, path: &str) {
    let Some(locator) = locator else { return };
    locator.part = Some(match locator.part.take() {
        Some(child) => format!("{path}/{child}"),
        None => path.into(),
    });
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("EPUB merge: {}", detail.into()) }
}
