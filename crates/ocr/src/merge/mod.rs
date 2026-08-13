//! Deterministic OCR geometry, policy, deduplication, and Document IR merge.

use crate::{PageDetection, RecognitionResult};
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity, Document,
    ExecutionContext, OcrOptions, OcrPolicy, SourceLocator,
};
use std::collections::BTreeSet;

#[cfg(test)]
type TraversalTestHook = Option<Box<dyn FnMut(usize)>>;

#[cfg(test)]
std::thread_local! {
    static TRAVERSAL_TEST_HOOK: std::cell::RefCell<TraversalTestHook> =
        std::cell::RefCell::new(None);
}

mod budget;
mod dedup;
mod geometry;
mod lines;
mod page_scope;
mod paragraphs;
mod policy;
mod provenance;

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;

use budget::MergeBudget;
use geometry::RegionGeometry;

/// Hard safety and work limits for OCR-to-IR merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeLimits {
    /// Maximum page inputs in one merge operation.
    pub max_pages: usize,
    /// Maximum detector regions across all pages.
    pub max_regions: usize,
    /// Maximum recognition UTF-8 bytes across all pages.
    pub max_text_bytes: usize,
    /// Maximum provider/model identity bytes across all pages.
    pub max_identity_bytes: usize,
    /// Maximum candidate comparisons across deduplication and clustering.
    pub max_comparisons: u64,
}

impl Default for MergeLimits {
    fn default() -> Self {
        Self {
            max_pages: 10_000,
            max_regions: 3_000,
            max_text_bytes: 16 * 1024 * 1024,
            max_identity_bytes: 64 * 1024,
            max_comparisons: 12_000_000,
        }
    }
}

/// Policy and safety controls for OCR-to-IR merge.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeConfig {
    /// Whether OCR is disabled, conditional on native text, or always considered.
    pub policy: OcrPolicy,
    /// Lowest detector and recognizer confidence accepted into the merge.
    pub minimum_confidence: f32,
    /// Printable native-character count at which automatic OCR is unnecessary.
    pub auto_min_native_characters: usize,
    /// Local merge safety limits.
    pub limits: MergeLimits,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            policy: OcrPolicy::Auto,
            minimum_confidence: 0.70,
            auto_min_native_characters: 8,
            limits: MergeLimits::default(),
        }
    }
}

impl From<&OcrOptions> for MergeConfig {
    fn from(options: &OcrOptions) -> Self {
        Self {
            policy: options.policy,
            minimum_confidence: options.minimum_confidence,
            ..Self::default()
        }
    }
}

/// One page's already-associated detector and recognizer results.
#[derive(Debug, Clone)]
pub struct OcrPageInput {
    /// Detector-produced, page-scoped batch in stable source-region order.
    detection: PageDetection,
    /// Recognition output whose `source_index` values address detector regions.
    recognition: RecognitionResult,
}

impl OcrPageInput {
    /// Create a merge input from detector- and recognizer-produced bound results.
    pub fn new(
        detection: PageDetection,
        recognition: RecognitionResult,
    ) -> Result<Self, ConversionError> {
        detection.identity.validate(&detection.result)?;
        if recognition.batch_identity.as_ref() != Some(&detection.identity)
            || recognition.recognizer_model != Some(crate::batch::RECOGNIZER_MODEL_ID)
        {
            return Err(ocr("batchIdentityMismatch"));
        }
        Ok(Self { detection, recognition })
    }

    fn page(&self) -> u32 {
        self.detection.identity.page
    }

    fn page_width(&self) -> f32 {
        self.detection.page_width()
    }

    fn page_height(&self) -> f32 {
        self.detection.page_height()
    }

    fn detected(&self) -> &crate::DetectionResult {
        &self.detection.result
    }
}

#[derive(Debug)]
pub(crate) struct Candidate {
    source_index: usize,
    text: String,
    detection_confidence: f32,
    recognition_confidence: f32,
    geometry: RegionGeometry,
}

#[derive(Clone, Copy)]
pub(crate) struct PageBlocks<'a> {
    pub(crate) blocks: &'a [BlockNode],
    pub(crate) explicitly_scoped: bool,
}

fn traversal_checkpoint(context: &ExecutionContext, visited: usize) -> Result<(), ConversionError> {
    if !visited.is_multiple_of(256) {
        return Ok(());
    }
    #[cfg(test)]
    TRAVERSAL_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(visited);
        }
    });
    context.checkpoint()
}

#[cfg(test)]
fn set_traversal_test_hook(hook: Option<Box<dyn FnMut(usize)>>) {
    TRAVERSAL_TEST_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

/// Merge OCR results into a consumed document and return one accounted unified IR output.
///
/// All validation, resource reservation, filtering, and deduplication completes
/// before the returned output can be published. On any failure the local
/// document and its reservation are dropped together.
pub fn merge_document(
    document: Document,
    pages: &[OcrPageInput],
    config: &MergeConfig,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    policy::validate(config)?;
    let planned_pages = if config.policy == OcrPolicy::Off { &[][..] } else { pages };
    let mut budget = MergeBudget::preflight(&document, planned_pages, config, context)?;
    let mut document = document;
    let mut diagnostics = Vec::new();
    diagnostics.try_reserve_exact(budget.planned_diagnostics()).map_err(|_| memory())?;
    let mut identifiers = BTreeSet::new();
    collect_node_ids(&document.blocks, &mut identifiers, &budget)?;
    if config.policy != OcrPolicy::Off {
        let mut ordered = Vec::<&OcrPageInput>::new();
        ordered.try_reserve_exact(pages.len()).map_err(|_| memory())?;
        ordered.extend(pages);
        ordered.sort_by_key(|page| page.page());
        for adjacent in ordered.windows(2) {
            if adjacent[0].page() == adjacent[1].page() {
                return Err(ocr(format!("duplicatePage:{}", adjacent[0].page())));
            }
        }
        for page in ordered {
            budget.checkpoint()?;
            merge_page(
                &mut document,
                page,
                config,
                &mut identifiers,
                &mut diagnostics,
                &mut budget,
            )?;
        }
    }
    drop(identifiers);
    document.validate().map_err(|error| {
        ocr(format!("invalidMergedDocument:{}:{}", error.code.as_str(), error.path))
    })?;
    let reservation = budget.finish()?;
    ConverterOutput::new_with_memory_reservation(
        document,
        Vec::new(),
        diagnostics,
        context,
        reservation,
    )
}

fn merge_page(
    document: &mut Document,
    page: &OcrPageInput,
    config: &MergeConfig,
    identifiers: &mut BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut MergeBudget<'_>,
) -> Result<(), ConversionError> {
    let page_blocks = existing_page_blocks(document, page.page())?;
    if config.policy == OcrPolicy::Auto
        && policy::has_sufficient_native_text(
            page_blocks.blocks,
            page.page(),
            page_blocks.explicitly_scoped,
            config.auto_min_native_characters,
            budget.context(),
        )?
    {
        return Ok(());
    }
    validate_page(page)?;
    let mut candidates = associate_candidates(page, config, diagnostics, budget)?;
    let native = dedup::collect_native_spans(
        page_blocks.blocks,
        page.page(),
        page_blocks.explicitly_scoped,
        budget,
    )?;
    candidates = dedup::suppress_duplicates(
        candidates,
        &native,
        page.page(),
        page.page_width(),
        page.page_height(),
        budget,
        diagnostics,
    )?;
    drop(native);
    if candidates.is_empty() {
        return Ok(());
    }
    let lines = lines::merge_lines(candidates, page.recognition.language_hint.as_deref(), budget)?;
    let paragraphs = paragraphs::merge_paragraphs(lines)?;
    let nodes = provenance::materialize_paragraphs(paragraphs, page, identifiers)?;
    insert_page_nodes(document, page, nodes, identifiers)
}

fn validate_page(page: &OcrPageInput) -> Result<(), ConversionError> {
    if page.page() == 0
        || !page.page_width().is_finite()
        || !page.page_height().is_finite()
        || page.page_width() <= 0.0
        || page.page_height() <= 0.0
    {
        return Err(ocr("invalidPageGeometry"));
    }
    page.detection.identity.validate(page.detected())?;
    if page.recognition.batch_identity.as_ref() != Some(&page.detection.identity)
        || page.recognition.recognizer_model != Some(crate::batch::RECOGNIZER_MODEL_ID)
    {
        return Err(ocr("batchIdentityMismatch"));
    }
    for identity in [
        page.detected().provider.as_str(),
        page.recognition.provider.as_ref(),
        page.detection.identity.detector_model,
        page.recognition.recognizer_model.unwrap_or(""),
    ] {
        if identity.trim().is_empty() || identity.chars().any(char::is_control) {
            return Err(ocr("invalidProviderOrModelIdentity"));
        }
    }
    if page.detected().regions.len() != page.recognition.regions.len() {
        return Err(ocr("detectionRecognitionCountMismatch"));
    }
    Ok(())
}

fn associate_candidates(
    page: &OcrPageInput,
    config: &MergeConfig,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut MergeBudget<'_>,
) -> Result<Vec<Candidate>, ConversionError> {
    let mut recognized = Vec::new();
    recognized.try_reserve_exact(page.detected().regions.len()).map_err(|_| memory())?;
    recognized.resize(page.detected().regions.len(), None);
    for item in page.recognition.regions.iter() {
        if item.source_index >= recognized.len() || recognized[item.source_index].is_some() {
            return Err(ocr("invalidRecognitionSourceIndex"));
        }
        recognized[item.source_index] = Some(item);
    }
    let mut candidates = Vec::new();
    candidates.try_reserve_exact(page.detected().regions.len()).map_err(|_| memory())?;
    for (source_index, region) in page.detected().regions.iter().enumerate() {
        budget.consume(1)?;
        if !region.confidence.is_finite() || !(0.0..=1.0).contains(&region.confidence) {
            return Err(ocr("invalidDetectionConfidence"));
        }
        let text = recognized[source_index].ok_or_else(|| ocr("missingRecognitionSourceIndex"))?;
        if !text.confidence.is_finite() || !(0.0..=1.0).contains(&text.confidence) {
            return Err(ocr("invalidRecognitionConfidence"));
        }
        let geometry = RegionGeometry::from_polygon(region.polygon)
            .ok_or_else(|| ocr("invalidDetectionPolygon"))?;
        if geometry.polygon.iter().any(|point| {
            point.x < 0.0
                || point.y < 0.0
                || point.x > page.page_width()
                || point.y > page.page_height()
        }) {
            return Err(ocr("detectionPolygonOutsidePage"));
        }
        let locator = SourceLocator {
            page: Some(page.page()),
            bounds: Some(geometry.bounds),
            page_width: Some(page.page_width()),
            page_height: Some(page.page_height()),
            ..SourceLocator::default()
        };
        if text.text.trim().is_empty() {
            diagnostics.push(diagnostic("ocr.emptyText", "OCR region produced no text", locator));
            continue;
        }
        if text.text.chars().any(|character| character.is_control() && !character.is_whitespace()) {
            return Err(ocr("invalidRecognitionText"));
        }
        if region.confidence < config.minimum_confidence
            || text.confidence < config.minimum_confidence
        {
            diagnostics.push(diagnostic(
                "ocr.lowConfidence",
                "OCR region was filtered below the configured minimum confidence",
                locator,
            ));
            continue;
        }
        let mut value = String::new();
        value.try_reserve_exact(text.text.len()).map_err(|_| memory())?;
        value.push_str(&text.text);
        candidates.push(Candidate {
            source_index,
            text: value,
            detection_confidence: region.confidence,
            recognition_confidence: text.confidence,
            geometry,
        });
    }
    Ok(candidates)
}

fn existing_page_blocks(document: &Document, page: u32) -> Result<PageBlocks<'_>, ConversionError> {
    let mut matching = None;
    let mut has_page_containers = false;
    for node in &document.blocks {
        let Block::Page { number, blocks } = &node.block else { continue };
        has_page_containers = true;
        if *number != page {
            continue;
        }
        if matching.replace(blocks.as_slice()).is_some() {
            return Err(ocr(format!("duplicateDocumentPage:{page}")));
        }
    }
    if matching.is_none() && has_page_containers {
        return Ok(PageBlocks { blocks: &[], explicitly_scoped: true });
    }
    if let Some(blocks) = matching {
        Ok(PageBlocks { blocks, explicitly_scoped: true })
    } else {
        Ok(PageBlocks { blocks: &document.blocks, explicitly_scoped: false })
    }
}

fn insert_page_nodes(
    document: &mut Document,
    page: &OcrPageInput,
    nodes: Vec<BlockNode>,
    identifiers: &mut BTreeSet<String>,
) -> Result<(), ConversionError> {
    if let Some(blocks) = document.blocks.iter_mut().find_map(|node| match &mut node.block {
        Block::Page { number, blocks } if *number == page.page() => Some(blocks),
        _ => None,
    }) {
        blocks.try_reserve_exact(nodes.len()).map_err(|_| memory())?;
        blocks.extend(nodes);
        sort_page_reading_order(blocks);
        return Ok(());
    }
    document.blocks.try_reserve_exact(1).map_err(|_| memory())?;
    document.blocks.push(BlockNode {
        id: provenance::unique_page_id(page.page(), identifiers)?,
        block: Block::Page { number: page.page(), blocks: nodes },
        provenance: provenance::page_provenance(page)?,
    });
    Ok(())
}

fn sort_page_reading_order(blocks: &mut [BlockNode]) {
    blocks.sort_by(|left, right| {
        match (left.provenance.locator.bounds, right.provenance.locator.bounds) {
            (Some(left), Some(right)) => left
                .y
                .total_cmp(&right.y)
                .then_with(|| left.x.total_cmp(&right.x))
                .then_with(|| left.height.total_cmp(&right.height))
                .then_with(|| left.width.total_cmp(&right.width)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

fn collect_node_ids(
    nodes: &[BlockNode],
    identifiers: &mut BTreeSet<String>,
    budget: &MergeBudget<'_>,
) -> Result<(), ConversionError> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(1).map_err(|_| memory())?;
    stack.push(nodes);
    let mut visited = 0_usize;
    while let Some(nodes) = stack.pop() {
        for node in nodes {
            visited += 1;
            traversal_checkpoint(budget.context(), visited)?;
            let mut identifier = String::new();
            identifier.try_reserve_exact(node.id.0.len()).map_err(|_| memory())?;
            identifier.push_str(&node.id.0);
            identifiers.insert(identifier);
            match &node.block {
                Block::List { items, .. } => {
                    for item in items {
                        stack.try_reserve(1).map_err(|_| memory())?;
                        stack.push(item.blocks.as_slice());
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        stack.try_reserve(1).map_err(|_| memory())?;
                        stack.push(cell.blocks.as_slice());
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => {
                    stack.try_reserve(1).map_err(|_| memory())?;
                    stack.push(blocks.as_slice());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn diagnostic(code: &str, message: &str, locator: SourceLocator) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        severity: DiagnosticSeverity::Warning,
        message: message.into(),
        locator: Some(locator),
    }
}

fn ocr(detail: impl Into<String>) -> ConversionError {
    ConversionError::Ocr { provider: provenance::MERGE_PROVIDER.into(), detail: detail.into() }
}

fn memory() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "OCR merge allocation exceeded its preflight plan".into(),
    }
}

fn limit(name: &'static str, observed: usize, maximum: usize) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: format!("{observed} > {maximum}") }
}
