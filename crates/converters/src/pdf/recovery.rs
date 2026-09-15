//! Bounded PDF recovery transactions with source-addressed delivery evidence.

use super::*;
use into_markdown_pdf_layout::{LayoutConfig, PagePathEvidence};

pub(super) fn plan_paths<'page>(
    page: &'page into_markdown_pdfium::Page<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<into_markdown_pdfium::PlannedPathBounds<'page, 'page>, ConversionError> {
    let plan =
        request_path_scan(context, |checkpoint| page.plan_path_bounds_with_checkpoint(checkpoint))?;
    if options.error_policy == ErrorPolicy::BestEffort && plan.contains_composite_visuals() {
        return Err(malformed(
            "pdf-layout-visual",
            "composite FORM or SHADING graphics require a page image",
        ));
    }
    Ok(plan)
}

/// Repeated unmapped glyphs make native transcription unreliable. Keep that
/// transcription as a supplement to the original page's visual representation.
pub(super) fn check_native_mapping(
    characters: impl Iterator<Item = char>,
    policy: ErrorPolicy,
) -> Result<(), ConversionError> {
    if policy == ErrorPolicy::BestEffort
        && characters
            .filter(|value| matches!(value, '\u{fffd}' | '\u{fffe}' | '\u{ffff}'))
            .take(3)
            .count()
            == 3
    {
        return Err(malformed(
            "pdf-layout-visual",
            "repeated unmapped native glyphs require a page image",
        ));
    }
    Ok(())
}

fn check_visual_paths(paths: &[PagePathEvidence]) -> Result<(), ConversionError> {
    // Numerous two-dimensional paths carry heatmap cells, bars and diagram
    // shapes. Thin table rules remain available to semantic reconstruction.
    if paths.iter().any(|page| {
        page.bounds
            .iter()
            .filter(|bounds| bounds.width > 2.0 && bounds.height > 2.0)
            .take(16)
            .count()
            == 16
    }) {
        return Err(malformed(
            "pdf-layout-visual",
            "dense two-dimensional vector graphics require a page image",
        ));
    }
    Ok(())
}

pub(crate) fn recoverable(policy: ErrorPolicy, error: &ConversionError) -> bool {
    policy == ErrorPolicy::BestEffort
        && !matches!(
            error,
            ConversionError::Cancelled
                | ConversionError::Timeout
                | ConversionError::Io { .. }
                | ConversionError::Network { .. }
                | ConversionError::ResourceLimit { limit: "pdfLayoutConfig", .. }
        )
}

pub(crate) fn diagnostic(
    page: Option<u32>,
    stage: &str,
    representation: &str,
    error: &ConversionError,
) -> Diagnostic {
    Diagnostic {
        code: format!("pdf.recovery.{representation}"),
        severity: DiagnosticSeverity::Warning,
        message: format!(
            "PDF {stage}: retained {representation}; {}",
            error.to_string().chars().take(512).collect::<String>()
        ),
        locator: Some(SourceLocator {
            page,
            part: Some(format!("pdf/{stage}")),
            ..SourceLocator::default()
        }),
    }
}

/// Keep a compact, bounded source-text snapshot while attempting semantic layout.
pub(crate) fn layout(
    document: Document,
    config: &LayoutConfig,
    paths: &[PagePathEvidence],
    policy: ErrorPolicy,
    context: &ExecutionContext,
) -> Result<(Document, Option<ResourceReservation>, Vec<Diagnostic>), ConversionError> {
    if policy == ErrorPolicy::Strict {
        let result = into_markdown_pdf_layout::reconstruct_document_with_path_evidence(
            document, config, paths, context,
        )?;
        let (document, reservation) = result.into_parts();
        return Ok((document, reservation, Vec::new()));
    }
    context.checkpoint()?;
    check_visual_paths(paths)?;
    let bytes = snapshot_bytes(&document.blocks).saturating_add(64 * 1024);
    let reservation = match context.reserve_memory(bytes) {
        Ok(reservation) => reservation,
        Err(error) if recoverable(policy, &error) => {
            let diagnostics = page_diagnostics(&document, "layout", "nativeText", &error);
            return Ok((document, None, diagnostics));
        }
        Err(error) => return Err(error),
    };
    let mut snapshot =
        Document { blocks: compact_nodes(&document.blocks, context)?, ..Document::default() };
    snapshot.metadata = document.metadata.clone();
    match into_markdown_pdf_layout::reconstruct_document_with_path_evidence(
        document, config, paths, context,
    ) {
        Ok(result) => {
            let (document, reservation) = result.into_parts();
            Ok((document, reservation, Vec::new()))
        }
        Err(error) if matches!(&error, ConversionError::Malformed { part, .. } if part.as_deref() == Some("pdf-layout-visual")) => {
            Err(error)
        }
        Err(error) if recoverable(policy, &error) => {
            context.checkpoint()?;
            let diagnostics = page_diagnostics(&snapshot, "layout", "nativeText", &error);
            snapshot.validate().map_err(|error| malformed("pdf-recovery", error.to_string()))?;
            Ok((snapshot, Some(reservation), diagnostics))
        }
        Err(error) => Err(error),
    }
}

/// Compact glyph provenance when retained output and delivery work compete
/// for the remaining request allowance. Each paragraph reuses its own scratch.
pub(crate) fn compact_for_delivery(
    mut output: ConverterOutput,
    policy: ErrorPolicy,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let validation = into_markdown_core::estimate_validation_working_set(
        &output.document,
        &output.assets,
        &output.diagnostics,
    )?;
    let retained = into_markdown_core::estimate_retained_output(
        &output.document,
        &output.assets,
        &output.diagnostics,
    )?;
    if policy == ErrorPolicy::Strict
        || validation.saturating_add(retained) <= context.available_memory_bytes()
    {
        return Ok(output);
    }
    let error = resource(
        "max_memory_bytes",
        format!(
            "PDF delivery holds {retained} bytes and needs {validation} validation bytes; compact page text retained"
        ),
    );
    output.diagnostics.extend(page_diagnostics(&output.document, "delivery", "nativeText", &error));
    compact_glyphs(&mut output.document.blocks, context)?;
    output.reconcile_retained_output(context)
}

fn compact_glyphs(
    nodes: &mut [BlockNode],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    fn inlines(
        values: &mut Vec<Inline>,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let _scratch = context.reserve_memory(inline_bytes(values))?;
        let count = inline_runs(values);
        let mut compact = Vec::new();
        compact
            .try_reserve_exact(count)
            .map_err(|_| resource("max_memory_bytes", "compact PDF inline runs"))?;
        let old = std::mem::replace(values, compact);
        for value in old {
            context.checkpoint()?;
            let value = match value {
                Inline::Text { value, marks }
                | Inline::SourceText { value, marks, .. }
                | Inline::OcrText { value, marks, .. } => {
                    if let Some(Inline::Text { value: text, marks: previous }) = values.last_mut()
                        && *previous == marks
                    {
                        text.try_reserve(value.len())
                            .map_err(|_| resource("max_memory_bytes", "compact PDF text run"))?;
                        text.push_str(&value);
                        continue;
                    }
                    Inline::Text { value, marks }
                }
                Inline::Link { target, mut content } => {
                    inlines(&mut content, context)?;
                    Inline::Link { target, content }
                }
                other => other,
            };
            values
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "compact PDF inline inventory"))?;
            values.push(value);
        }
        Ok(())
    }
    for node in nodes {
        context.checkpoint()?;
        match &mut node.block {
            Block::Paragraph(values)
            | Block::Heading { content: values, .. }
            | Block::TimedSegment { content: values, .. } => inlines(values, context)?,
            Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => compact_glyphs(blocks, context)?,
            Block::List { items, .. } => {
                for item in items {
                    compact_glyphs(&mut item.blocks, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter_mut().flat_map(|r| &mut r.cells) {
                    compact_glyphs(&mut cell.blocks, context)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn page_diagnostics(
    document: &Document,
    stage: &str,
    representation: &str,
    error: &ConversionError,
) -> Vec<Diagnostic> {
    document
        .blocks
        .iter()
        .map(|node| diagnostic(node.provenance.locator.page, stage, representation, error))
        .collect()
}

fn snapshot_bytes(nodes: &[BlockNode]) -> u64 {
    nodes.iter().fold(0_u64, |total, node| {
        let nested = match &node.block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => inline_bytes(inlines),
            Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => snapshot_bytes(blocks),
            Block::List { items, .. } => {
                items.iter().fold(0_u64, |n, item| n.saturating_add(snapshot_bytes(&item.blocks)))
            }
            Block::Table { rows, .. } => rows
                .iter()
                .flat_map(|r| &r.cells)
                .fold(0_u64, |n, c| n.saturating_add(snapshot_bytes(&c.blocks))),
            Block::Code { text, .. } | Block::Formula(text) => text.len() as u64 * 4,
            _ => 4096,
        };
        total.saturating_add(nested).saturating_add(4096)
    })
}

fn inline_runs(inlines: &[Inline]) -> usize {
    let mut previous = None;
    let mut runs = 0;
    for value in inlines {
        let marks = match value {
            Inline::Text { marks, .. }
            | Inline::SourceText { marks, .. }
            | Inline::OcrText { marks, .. } => Some(marks),
            _ => None,
        };
        if marks.is_none() || previous != marks {
            runs += 1;
        }
        previous = marks;
    }
    runs
}

fn inline_bytes(inlines: &[Inline]) -> u64 {
    let headers =
        (inline_runs(inlines) as u64).saturating_mul(std::mem::size_of::<Inline>() as u64);
    inlines.iter().fold(headers, |total, inline| {
        total.saturating_add(match inline {
            Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Text { value, .. } => value.len() as u64 * 4,
            Inline::Code(text) | Inline::Formula(text) | Inline::FootnoteReference(text) => {
                text.len() as u64 * 4 + 16
            }
            Inline::Link { target, content } => {
                inline_bytes(content).saturating_add(target.len() as u64 * 4 + 16)
            }
            _ => 8,
        })
    })
}

fn compact_nodes(
    nodes: &[BlockNode],
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(nodes.len())
        .map_err(|_| resource("max_memory_bytes", "PDF recovery nodes"))?;
    for node in nodes {
        context.checkpoint()?;
        let block = match &node.block {
            Block::Page { number, blocks } => {
                Block::Page { number: *number, blocks: compact_nodes(blocks, context)? }
            }
            Block::Image { .. } => node.block.clone(),
            _ => {
                let mut text = String::new();
                text.try_reserve(snapshot_bytes(std::slice::from_ref(node)) as usize)
                    .map_err(|_| resource("max_memory_bytes", "PDF recovery text"))?;
                append_block_text(&node.block, &mut text);
                Block::Paragraph(vec![Inline::Text { value: text, marks: Vec::new() }])
            }
        };
        output.push(BlockNode { id: node.id.clone(), block, provenance: node.provenance.clone() });
    }
    Ok(output)
}

fn append_inlines(inlines: &[Inline], text: &mut String) {
    for inline in inlines {
        match inline {
            Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Text { value, .. } => text.push_str(value),
            Inline::Code(value) | Inline::Formula(value) => text.push_str(value),
            Inline::FootnoteReference(value) => {
                text.push('[');
                text.push_str(value);
                text.push(']');
            }
            Inline::LineBreak => text.push('\n'),
            Inline::Link { content, target } => {
                append_inlines(content, text);
                text.push_str(" (");
                text.push_str(target);
                text.push(')');
            }
            _ => {}
        }
    }
}

fn append_block_text(block: &Block, text: &mut String) {
    match block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => append_inlines(inlines, text),
        Block::Code { text: value, .. } | Block::Formula(value) => text.push_str(value),
        Block::List { items, .. } => {
            for item in items {
                for node in &item.blocks {
                    append_block_text(&node.block, text);
                    text.push('\n');
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &row.cells {
                    for node in &cell.blocks {
                        append_block_text(&node.block, text);
                    }
                    text.push('\t');
                }
                text.push('\n');
            }
        }
        Block::Footnote { label, blocks } => {
            text.push_str(label);
            text.push(' ');
            for node in blocks {
                append_block_text(&node.block, text);
            }
        }
        _ => {}
    }
}

pub(crate) fn unavailable_runtime(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
    error: &ConversionError,
) -> Result<ConverterOutput, ConversionError> {
    if !recoverable(options.error_policy, error) {
        return Err(error.clone());
    }
    let original = Original::prepare(input, context)?;
    let page = original.page(None, 0, "runtime", error, options, context)?;
    original.attach(page, context)
}

/// Original bytes remain available until all page transactions have committed.
/// A single content-addressed attachment serves every page needing source access.
pub(super) struct Original {
    asset: Asset,
    reservation: ResourceReservation,
}

impl Original {
    pub(super) fn prepare(
        input: &ResolvedInput,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let bytes = input.bytes.len() as u64;
        let reservation = context.reserve_memory(bytes.saturating_add(64 * 1024))?;
        let id = content_asset_id("pdf-original", &input.bytes)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(input.bytes.len())
            .map_err(|_| resource("max_memory_bytes", "original PDF recovery copy"))?;
        payload.extend_from_slice(&input.bytes);
        Ok(Self {
            asset: Asset {
                id: AssetId(id.clone()),
                filename: Some(format!("{id}.pdf")),
                media_type: "application/pdf".into(),
                bytes: payload,
                external_uri: None,
            },
            reservation,
        })
    }

    /// Preserve the original as the bounded backing store when accumulated
    /// published images would consume the next page's recovery allowance.
    pub(super) fn prepare_delivery(
        &self,
        mut output: ConverterOutput,
        options: &ConversionOptions,
        published: &mut HashSet<AssetId>,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        let headroom = snapshot_bytes(&output.document.blocks).saturating_add(32 * 1024 * 1024);
        if options.error_policy == ErrorPolicy::BestEffort
            && !output.assets.is_empty()
            && context.available_memory_bytes() < headroom
        {
            let references = image_references(&output.document.blocks);
            let lease =
                context.reserve_memory(references.saturating_mul(1024).saturating_add(65536))?;
            let error = resource(
                "max_memory_bytes",
                "published images moved to the original PDF to preserve bounded page recovery and delivery headroom",
            );
            for node in &mut output.document.blocks {
                if replace_images_with_original(
                    std::slice::from_mut(node),
                    &self.asset.id,
                    context,
                )? {
                    output.diagnostics.push(diagnostic(
                        node.provenance.locator.page,
                        "asset-memory",
                        "originalPdf",
                        &error,
                    ));
                }
            }
            output.assets.clear();
            published.clear();
            output.attach_memory_reservation(context, lease)?;
            output = output.reconcile_retained_output(context)?;
        }
        compact_for_delivery(output, options.error_policy, context)
    }

    pub(super) fn attach(
        self,
        mut output: ConverterOutput,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        if output.diagnostics.iter().any(|d| d.code == "pdf.recovery.originalPdf") {
            output.document.metadata.properties.insert(
                "pdf.original.sha256".into(),
                format!("{:x}", Sha256::digest(&self.asset.bytes)),
            );
            output.assets.push(self.asset);
            output.attach_memory_reservation(context, self.reservation)?;
        }
        output.account_retained(context)
    }

    pub(super) fn page(
        &self,
        pdf: Option<&into_markdown_pdfium::Document<'_>>,
        index: u32,
        stage: &str,
        error: &ConversionError,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        if !recoverable(options.error_policy, error) {
            return Err(error.clone());
        }
        context.checkpoint()?;
        let page_number = index + 1;
        let known_page = pdf.is_some() || !matches!(stage, "runtime" | "document");
        let reservation = context.reserve_memory(64 * 1024)?;
        let locator = SourceLocator {
            page: known_page.then_some(page_number),
            part: Some(format!("pdf/{stage}")),
            ..SourceLocator::default()
        };
        let provenance = Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "builtin.pdf.recovery".into(),
            locator,
            confidence: None,
        };
        let RecoveredPage {
            mut blocks,
            assets,
            mut leases,
            mut representation,
            mut recovery_error,
        } = recover_page_content(pdf, index, error, options, context, &provenance)?;
        leases.push(reservation);
        if representation != "pageImage" {
            if options.output.asset_mode == AssetMode::Omit {
                representation = "assetsOmitted";
                recovery_error = malformed(
                    "pdf-recovery",
                    format!(
                        "{recovery_error}; source attachment omitted by the requested asset mode"
                    ),
                );
            } else {
                representation = "originalPdf";
                blocks.push(BlockNode {
                    id: NodeId(format!("pdf-page-{page_number}-original")),
                    block: Block::Image {
                        asset: self.asset.id.clone(),
                        alt: Some(if known_page {
                            format!("Original PDF — page {page_number}")
                        } else {
                            "Original PDF — complete source (all pages)".into()
                        }),
                    },
                    provenance: provenance.clone(),
                });
            }
        }
        let diagnostic =
            diagnostic(known_page.then_some(page_number), stage, representation, &recovery_error);
        place_recovery_note(&mut blocks, page_number, &diagnostic, &provenance)?;
        ConverterOutput::new_with_memory_reservations(
            Document {
                blocks: if known_page {
                    vec![BlockNode {
                        id: NodeId(format!("pdf-page-{page_number}")),
                        block: Block::Page { number: page_number, blocks },
                        provenance,
                    }]
                } else {
                    blocks
                },
                ..Document::default()
            },
            assets,
            vec![diagnostic],
            leases,
        )
        .account_retained(context)
    }
}

fn place_recovery_note(
    blocks: &mut Vec<BlockNode>,
    page_number: u32,
    diagnostic: &Diagnostic,
    provenance: &Provenance,
) -> Result<(), ConversionError> {
    let position = if let Some(index) =
        blocks.iter().position(|node| matches!(node.block, Block::Image { .. }))
    {
        blocks.swap(0, index);
        1
    } else {
        0
    };
    blocks.try_reserve(1).map_err(|_| resource("max_memory_bytes", "PDF recovery note"))?;
    blocks.insert(
        position,
        BlockNode {
            id: NodeId(format!("pdf-page-{page_number}-recovery-note")),
            block: Block::Paragraph(vec![Inline::Text {
                value: diagnostic.message.clone(),
                marks: Vec::new(),
            }]),
            provenance: provenance.clone(),
        },
    );
    if let Some(index) =
        blocks.iter().position(|node| node.id.0 == format!("pdf-page-{page_number}-recovered-text"))
    {
        blocks.try_reserve(1).map_err(|_| resource("max_memory_bytes", "PDF native text label"))?;
        blocks.insert(
            index,
            BlockNode {
                id: NodeId(format!("pdf-page-{page_number}-native-text-label")),
                block: Block::Heading {
                    level: 4,
                    content: vec![Inline::Text {
                        value: "Native page text".into(),
                        marks: Vec::new(),
                    }],
                },
                provenance: provenance.clone(),
            },
        );
    }
    Ok(())
}

fn image_references(nodes: &[BlockNode]) -> u64 {
    nodes
        .iter()
        .map(|node| match &node.block {
            Block::Image { .. } => 1,
            Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => image_references(blocks),
            Block::List { items, .. } => items.iter().map(|i| image_references(&i.blocks)).sum(),
            Block::Table { rows, .. } => {
                rows.iter().flat_map(|r| &r.cells).map(|c| image_references(&c.blocks)).sum()
            }
            _ => 0,
        })
        .sum()
}

fn replace_images_with_original(
    nodes: &mut [BlockNode],
    original: &AssetId,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut changed = false;
    for node in nodes {
        context.checkpoint()?;
        changed |= match &mut node.block {
            Block::Image { asset, alt } if asset != original => {
                *asset = original.clone();
                *alt = Some(format!(
                    "Original PDF page {} — image preserved in source (memory budget)",
                    node.provenance.locator.page.unwrap_or(1)
                ));
                if node.id.0.ends_with("-ocr-render") {
                    node.id.0.push_str("-source");
                }
                true
            }
            Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                replace_images_with_original(blocks, original, context)?
            }
            Block::List { items, .. } => {
                let mut any = false;
                for item in items {
                    any |= replace_images_with_original(&mut item.blocks, original, context)?;
                }
                any
            }
            Block::Table { rows, .. } => {
                let mut any = false;
                for cell in rows.iter_mut().flat_map(|r| &mut r.cells) {
                    any |= replace_images_with_original(&mut cell.blocks, original, context)?;
                }
                any
            }
            _ => false,
        };
    }
    Ok(changed)
}

struct RecoveredPage {
    blocks: Vec<BlockNode>,
    assets: Vec<Asset>,
    leases: Vec<ResourceReservation>,
    representation: &'static str,
    recovery_error: ConversionError,
}

fn recover_page_content(
    pdf: Option<&into_markdown_pdfium::Document<'_>>,
    index: u32,
    error: &ConversionError,
    options: &ConversionOptions,
    context: &ExecutionContext,
    provenance: &Provenance,
) -> Result<RecoveredPage, ConversionError> {
    let page_number = index + 1;
    let mut blocks = Vec::new();
    let mut assets = Vec::new();
    let mut leases = Vec::new();
    let mut representation = "originalPdf";
    let mut recovery_error = error.clone();
    if let Some(pdf) = pdf
        && let Ok(page) = pdf.page(index)
    {
        let mut provenance = provenance.clone();
        if let Ok(info) = page.info() {
            let (width, height) = super::geometry::displayed_dimensions(&info);
            provenance.locator.page_width = Some(width);
            provenance.locator.page_height = Some(height);
            provenance.locator.rotation_degrees = Some(0.0);
            provenance.locator.bounds = Some(Rect { x: 0.0, y: 0.0, width, height });
        }
        if let Ok(text_page) = page.text_page()
            && let Ok(count) = text_page.character_count()
            && let Ok(lease) = context
                .reserve_memory((u64::from(count) + 1).saturating_mul(12).saturating_add(4096))
            && let Ok(text) = text_page.text()
            && !text.trim().is_empty()
        {
            leases.push(lease);
            blocks.push(BlockNode { id: NodeId(format!("pdf-page-{page_number}-recovered-text")),
                block: if matches!(error, ConversionError::Malformed { part, .. } if part.as_deref() == Some("pdf-layout-visual")) {
                    Block::Code { language: Some("text".into()), text }
                } else { Block::Paragraph(vec![Inline::Text { value: text, marks: Vec::new() }]) }, provenance: provenance.clone() });
            representation = "nativeText";
        }
        if options.output.asset_mode != AssetMode::Omit {
            match render_recovery_page(&page, options, context) {
                Ok((asset, lease)) => {
                    blocks.push(BlockNode {
                        id: NodeId(format!("pdf-page-{page_number}-recovery-image")),
                        block: Block::Image {
                            asset: asset.id.clone(),
                            alt: Some(format!("Original page {page_number}")),
                        },
                        provenance: provenance.clone(),
                    });
                    assets.push(asset);
                    leases.push(lease);
                    representation = "pageImage";
                }
                Err(render_error) => {
                    if !recoverable(options.error_policy, &render_error) {
                        return Err(render_error);
                    }
                    recovery_error = malformed(
                        "pdf-recovery",
                        format!("{error}; page rendering: {render_error}"),
                    );
                }
            }
        }
    }
    Ok(RecoveredPage { blocks, assets, leases, representation, recovery_error })
}

fn render_recovery_page(
    page: &into_markdown_pdfium::Page<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Asset, ResourceReservation), ConversionError> {
    use image::ImageEncoder as _;
    let info = page.info().map_err(map_pdfium_error)?;
    let (width, height) = render_dimensions(&info)?;
    let bytes = u64::from(width).saturating_mul(u64::from(height)).saturating_mul(4);
    let mut reservation = context.reserve_memory(bytes.saturating_mul(4).saturating_add(65536))?;
    let mut bitmap = page.render_bgra(width, height).map_err(map_pdfium_error)?;
    if bitmap.bytes.len() as u64 != bytes {
        return Err(malformed("pdf-recovery-png", "unexpected bitmap stride"));
    }
    context.checkpoint()?;
    for pixel in bitmap.bytes.chunks_exact_mut(4) {
        opaque_rgba(pixel);
    }
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(bytes as usize + 65536)
        .map_err(|_| resource("max_memory_bytes", "PDF recovery PNG"))?;
    let capacity = encoded.capacity();
    let mut writer = BoundedPng { bytes: &mut encoded, capacity };
    image::codecs::png::PngEncoder::new(&mut writer)
        .write_image(&bitmap.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| malformed("pdf-recovery-png", error.to_string()))?;
    if encoded.len() as u64 > options.limits.max_asset_bytes {
        return Err(resource("max_asset_bytes", "PDF recovery PNG"));
    }
    drop(bitmap);
    // Retain compressed bytes; release the encoder workspace before the next page.
    encoded.shrink_to_fit();
    let retained = encoded.capacity() as u64 + 4096;
    reservation.shrink(bytes.saturating_mul(4).saturating_add(65536).saturating_sub(retained))?;
    let id = content_asset_id("pdf-recovery-page", &encoded)?;
    Ok((
        Asset {
            id: AssetId(id.clone()),
            filename: Some(format!("{id}.png")),
            media_type: "image/png".into(),
            bytes: encoded,
            external_uri: None,
        },
        reservation,
    ))
}

fn opaque_rgba(pixel: &mut [u8]) {
    pixel.swap(0, 2);
    let alpha = u16::from(pixel[3]);
    for channel in &mut pixel[..3] {
        *channel = ((u16::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
    }
    pixel[3] = 255;
}

struct BoundedPng<'a> {
    bytes: &'a mut Vec<u8>,
    capacity: usize,
}
impl std::io::Write for BoundedPng<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.capacity.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("PDF recovery PNG exceeded its allocation plan"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};
    use std::sync::Arc;

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    fn native_composite_render_preserves_red_blue_and_white_pixels() {
        let runtime = Pdfium::load_pinned(
            Path::new(&std::env::var_os("PDFIUM_LIBRARY").unwrap()),
            Limits::default(),
        )
        .unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let input = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/pdf/composite-colors.pdf").as_slice(),
            ),
            metadata: SourceMetadata::default(),
        };
        let pdf = open_document(&runtime, &input, &context).unwrap();
        let page = pdf.page(0).unwrap();
        assert!(page.plan_path_bounds().unwrap().contains_composite_visuals());
        let (asset, _lease) =
            render_recovery_page(&page, &ConversionOptions::default(), &context).unwrap();
        let image = image::load_from_memory(&asset.bytes).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (600, 600));
        assert_eq!(image.get_pixel(100, 500).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(400, 500).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    fn compressed_recovery_pages_share_a_small_retained_budget() {
        let runtime = Pdfium::load_pinned(
            Path::new(&std::env::var_os("PDFIUM_LIBRARY").unwrap()),
            Limits::default(),
        )
        .unwrap();
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 8 * 1024 * 1024, ..ResourceLimits::default() },
        );
        let input = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/pdf/composite-colors.pdf").as_slice(),
            ),
            metadata: SourceMetadata::default(),
        };
        let pdf = open_document(&runtime, &input, &context).unwrap();
        let page = pdf.page(0).unwrap();
        let mut retained = Vec::new();
        for _ in 0..32 {
            retained.push(
                render_recovery_page(&page, &ConversionOptions::default(), &context).unwrap(),
            );
        }
        assert!(context.reserved_memory_bytes() < 1024 * 1024);
        assert!(retained.windows(2).all(|pair| pair[0].0.bytes == pair[1].0.bytes));
        drop(retained);
        drop(page);
        drop(pdf);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn dense_vector_cells_request_visual_recovery_while_rules_keep_layout() {
        let mut paths = vec![PagePathEvidence {
            page: 1,
            bounds: vec![Rect { x: 10.0, y: 10.0, width: 8.0, height: 6.0 }; 16],
        }];
        assert!(check_visual_paths(&paths).is_err());
        for bounds in &mut paths[0].bounds {
            bounds.height = 0.5;
        }
        assert!(check_visual_paths(&paths).is_ok());
    }

    #[test]
    fn repeated_unmapped_glyphs_request_visual_recovery() {
        assert!(
            check_native_mapping(
                "body \u{fffd} cell \u{fffe} cell \u{ffff}".chars(),
                ErrorPolicy::BestEffort
            )
            .is_err()
        );
        assert!(
            check_native_mapping("one \u{fffd} example".chars(), ErrorPolicy::BestEffort).is_ok()
        );
        assert!(
            check_native_mapping("\u{fffd}\u{fffd}\u{fffd}".chars(), ErrorPolicy::Strict).is_ok()
        );
    }

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    fn render_refusal_retains_original_bytes_and_known_page() {
        let path = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").unwrap());
        let runtime =
            Pdfium::load_pinned(&path, Limits { max_render_pixels: 1, ..Limits::default() })
                .unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let input = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../../../fixtures/small/pdf/multicolumn.pdf").as_slice(),
            ),
            metadata: SourceMetadata::default(),
        };
        let pdf = open_document(&runtime, &input, &context).unwrap();
        let original = Original::prepare(&input, &context).unwrap();
        let page = original
            .page(
                Some(&pdf),
                0,
                "layout",
                &malformed("layout", "injected stage refusal"),
                &ConversionOptions::default(),
                &context,
            )
            .unwrap();
        let output = original.attach(page, &context).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes, input.bytes.as_ref());
        assert_eq!(output.diagnostics[0].code, "pdf.recovery.originalPdf");
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().page, Some(1));
        assert!(output.diagnostics[0].message.contains("max_render_pixels"));
        output.document.validate().unwrap();
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn image_pressure_keeps_one_original_and_releases_published_payloads() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 12 * 1024 * 1024, ..ResourceLimits::default() },
        );
        let input = ResolvedInput {
            bytes: Arc::from(b"%PDF-source".as_slice()),
            metadata: SourceMetadata::default(),
        };
        let original = Original::prepare(&input, &context).unwrap();
        let provenance = Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: PROVIDER_ID.into(),
            locator: SourceLocator { page: Some(1), ..SourceLocator::default() },
            confidence: None,
        };
        let asset_id = AssetId("large-image".into());
        let source = ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("page".into()),
                    block: Block::Page {
                        number: 1,
                        blocks: vec![BlockNode {
                            id: NodeId("image".into()),
                            block: Block::Image { asset: asset_id.clone(), alt: None },
                            provenance: provenance.clone(),
                        }],
                    },
                    provenance,
                }],
                ..Document::default()
            },
            vec![Asset {
                id: asset_id.clone(),
                filename: Some("large.png".into()),
                media_type: "image/png".into(),
                bytes: vec![0; 8 * 1024 * 1024],
                external_uri: None,
            }],
            vec![],
        )
        .account_retained(&context)
        .unwrap();
        let mut published = HashSet::from([asset_id]);
        let recovered = original
            .prepare_delivery(source, &ConversionOptions::default(), &mut published, &context)
            .unwrap();
        assert!(published.is_empty());
        assert!(recovered.assets.is_empty());
        assert!(context.reserved_memory_bytes() < 1024 * 1024);
        let output = original.attach(recovered, &context).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes, input.bytes.as_ref());
        assert_eq!(output.diagnostics[0].code, "pdf.recovery.originalPdf");
        output.document.validate().unwrap();
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn recovery_pages_use_opaque_white_paper_and_correct_color_channels() {
        let mut transparent = [0, 0, 0, 0];
        opaque_rgba(&mut transparent);
        assert_eq!(transparent, [255, 255, 255, 255]);
        let mut opaque = [11, 22, 33, 255];
        opaque_rgba(&mut opaque);
        assert_eq!(opaque, [33, 22, 11, 255]);
        let mut antialias = [0, 0, 0, 128];
        opaque_rgba(&mut antialias);
        assert_eq!(antialias, [127, 127, 127, 255]);
    }

    #[test]
    fn original_pdf_recovery_keeps_bytes_page_and_degraded_evidence() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let bytes: Arc<[u8]> = Arc::from(b"%PDF-1.7\nsource fixture".as_slice());
        let input =
            ResolvedInput { bytes: Arc::clone(&bytes), metadata: SourceMetadata::default() };
        let original = Original::prepare(&input, &context).unwrap();
        let error = malformed("fixture", "page rendering unavailable");
        let page = original
            .page(None, 6, "extraction", &error, &ConversionOptions::default(), &context)
            .unwrap();
        let output = original.attach(page, &context).unwrap();
        output.document.validate().unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes.as_slice(), bytes.as_ref());
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().page, Some(7));
        assert_eq!(
            into_markdown_core::conversion_outcome(&output.diagnostics),
            into_markdown_core::ConversionOutcome::Degraded
        );
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn delivery_compaction_preserves_structure_links_and_words() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1024 * 1024, ..ResourceLimits::default() },
        );
        let provenance = Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: PROVIDER_ID.into(),
            locator: SourceLocator::default(),
            confidence: None,
        };
        let words = "Readable body text ".repeat(128);
        let mut content = words
            .chars()
            .map(|c| Inline::SourceText {
                value: c.to_string(),
                marks: vec![],
                provenance: Box::new(provenance.clone()),
            })
            .collect::<Vec<_>>();
        content.push(Inline::Link {
            target: "https://example.test/source".into(),
            content: vec![Inline::Text { value: "source".into(), marks: vec![] }],
        });
        let source = ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("heading".into()),
                    block: Block::Heading { level: 2, content },
                    provenance,
                }],
                ..Document::default()
            },
            vec![],
            vec![],
        );
        let source = source.reconcile_retained_output(&context).unwrap();
        let pressure = context.reserve_memory(context.available_memory_bytes() / 2).unwrap();
        let output = compact_for_delivery(source, ErrorPolicy::BestEffort, &context).unwrap();
        let Block::Heading { level, content } = &output.document.blocks[0].block else {
            panic!("heading")
        };
        assert_eq!(*level, 2);
        assert!(matches!(&content[0], Inline::Text { value, .. } if value == &words));
        assert!(
            matches!(&content[1], Inline::Link { target, .. } if target == "https://example.test/source")
        );
        assert_eq!(output.diagnostics[0].code, "pdf.recovery.nativeText");
        drop(output);
        drop(pressure);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn terminal_controls_and_strict_remain_terminal() {
        for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
            assert!(!recoverable(policy, &ConversionError::Cancelled));
            assert!(!recoverable(policy, &ConversionError::Timeout));
        }
        assert!(!recoverable(ErrorPolicy::Strict, &malformed("fixture", "invalid")));
        assert!(recoverable(ErrorPolicy::BestEffort, &malformed("fixture", "invalid")));
    }

    #[test]
    fn exhausted_layout_keeps_source_words_with_a_bounded_lease() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let provenance = Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: PROVIDER_ID.into(),
            locator: SourceLocator {
                page: Some(1),
                page_width: Some(600.0),
                page_height: Some(800.0),
                ..SourceLocator::default()
            },
            confidence: None,
        };
        let source = Document {
            blocks: vec![BlockNode {
                id: NodeId("pdf-page-1".into()),
                block: Block::Page {
                    number: 1,
                    blocks: vec![BlockNode {
                        id: NodeId("body".into()),
                        block: Block::Paragraph(
                            "All original words"
                                .chars()
                                .enumerate()
                                .map(|(index, character)| Inline::SourceText {
                                    value: character.to_string(),
                                    marks: vec![],
                                    provenance: Box::new(Provenance {
                                        locator: SourceLocator {
                                            bounds: Some(Rect {
                                                x: 20.0 + index as f32 * 8.0,
                                                y: 50.0,
                                                width: 8.0,
                                                height: 12.0,
                                            }),
                                            ..provenance.locator.clone()
                                        },
                                        ..provenance.clone()
                                    }),
                                })
                                .collect(),
                        ),
                        provenance: provenance.clone(),
                    }],
                },
                provenance,
            }],
            ..Document::default()
        };
        let mut config = LayoutConfig::default();
        config.limits.max_atoms = 1;
        let (document, lease, diagnostics) =
            layout(source, &config, &[], ErrorPolicy::BestEffort, &context).unwrap();
        assert!(document.to_json().unwrap().contains("All original words"));
        assert_eq!(diagnostics[0].code, "pdf.recovery.nativeText");
        document.validate().unwrap();
        drop(document);
        drop(lease);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}
