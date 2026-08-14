use crate::{
    Asset, BlockNode, ConversionError, ConversionOptions, Diagnostic, Document, ExecutionContext,
    ExecutionOptions, FormatCandidate, FormatHint, InputFormat, InputRef, OcrRecognition,
    OcrResult, Provenance, ResolvedInput, ResolvedSource, ResourceReservation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::future::Future;
use std::mem::size_of;
use std::pin::Pin;
use std::sync::Arc;

/// Sendable boxed future used to keep service-provider traits object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Complete conversion request.
#[derive(Debug, Clone)]
pub struct ConversionRequest {
    /// Source to resolve.
    pub input: InputRef,
    /// Optional format hints.
    pub hint: FormatHint,
    /// Pipeline policy.
    pub options: ConversionOptions,
    /// Cancellation, timeout, and progress controls for this invocation.
    pub execution: ExecutionOptions,
}

/// Request to resolve and detect an input without converting it.
#[derive(Debug, Clone)]
pub struct DetectionRequest {
    /// Source to resolve.
    pub input: InputRef,
    /// Optional format hints.
    pub hint: FormatHint,
    /// Source, network, and resource policy.
    pub options: ConversionOptions,
    /// Cancellation, timeout, and progress controls for this invocation.
    pub execution: ExecutionOptions,
}

impl DetectionRequest {
    /// Construct a detection request with safe offline defaults.
    #[must_use]
    pub fn new(input: InputRef) -> Self {
        Self {
            input,
            hint: FormatHint::default(),
            options: ConversionOptions::default(),
            execution: ExecutionOptions::default(),
        }
    }
}

/// Format hypotheses and safe source metadata returned by detection.
#[derive(Debug, Clone)]
pub struct DetectionResult {
    /// Metadata produced by the selected source resolver.
    pub source: crate::SourceMetadata,
    /// Ordered format candidates.
    pub candidates: Vec<FormatCandidate>,
}

impl ConversionRequest {
    /// Construct a request with safe offline defaults.
    #[must_use]
    pub fn new(input: InputRef) -> Self {
        Self {
            input,
            hint: FormatHint::default(),
            options: ConversionOptions::default(),
            execution: ExecutionOptions::default(),
        }
    }
}

/// Final conversion result.
///
/// Its live-memory ownership is intentionally neither public nor cloneable:
///
/// ```compile_fail
/// # use into_markdown_core::{ConversionResult, Document};
/// let result = ConversionResult::new(Document::default(), String::new(), vec![], vec![], vec![]);
/// let detached = result.memory_lease;
/// # let _ = detached;
/// ```
///
/// ```compile_fail
/// # use into_markdown_core::{ConversionResult, Document};
/// let result = ConversionResult::new(Document::default(), String::new(), vec![], vec![], vec![]);
/// let copy = result.clone();
/// # let _ = copy;
/// ```
///
/// ```compile_fail
/// use into_markdown_core::OutputMemoryLease;
/// let forged = OutputMemoryLease::default();
/// # let _ = forged;
/// ```
#[derive(Debug)]
pub struct ConversionResult {
    /// Structured document before rendering.
    pub document: Document,
    /// GitHub-Flavored Markdown.
    pub markdown: String,
    /// Embedded/external resources.
    pub assets: Vec<Asset>,
    /// Non-fatal diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Ordered material provenance records for auditing.
    pub provenance: Vec<Provenance>,
    /// Live request-memory charges for retained IR and assets.
    pub(crate) memory_lease: OutputMemoryLease,
}

/// Opaque live-memory ownership transferred across conversion pipeline stages.
#[doc(hidden)]
#[derive(Debug, Default)]
pub(crate) struct OutputMemoryLease {
    leases: Vec<ResourceReservation>,
    accounted_bytes: u64,
}

impl OutputMemoryLease {
    /// Wrap reservations without exposing a detachable collection.
    #[doc(hidden)]
    #[must_use]
    fn from_reservations(leases: Vec<ResourceReservation>) -> Self {
        Self { leases, accounted_bytes: 0 }
    }

    fn from_reservation(lease: ResourceReservation) -> Result<Self, ConversionError> {
        let mut leases = Vec::new();
        leases.try_reserve_exact(1).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "output memory lease inventory allocation failed".into(),
        })?;
        leases.push(lease);
        Ok(Self::from_reservations(leases))
    }

    /// Add one reservation while preserving opaque ownership.
    #[doc(hidden)]
    fn push(&mut self, lease: ResourceReservation) -> Result<(), ConversionError> {
        self.leases.try_reserve(1).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "output memory lease inventory allocation failed".into(),
        })?;
        self.leases.push(lease);
        Ok(())
    }

    /// Bytes charged to this exact execution context.
    #[doc(hidden)]
    #[must_use]
    fn bytes_for(&self, context: &ExecutionContext) -> u64 {
        self.leases
            .iter()
            .filter(|lease| lease.belongs_to_memory_context(context))
            .map(ResourceReservation::bytes)
            .fold(0_u64, u64::saturating_add)
            .min(self.accounted_bytes)
    }
}

impl ConversionResult {
    /// Construct a result without transferred request-memory ownership.
    #[must_use]
    pub fn new(
        document: Document,
        markdown: String,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<Provenance>,
    ) -> Self {
        Self::from_accounted_parts(
            document,
            markdown,
            assets,
            diagnostics,
            provenance,
            OutputMemoryLease::default(),
        )
    }

    /// Assemble a result while retaining its opaque live-memory ownership.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    fn from_accounted_parts(
        document: Document,
        markdown: String,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<Provenance>,
        memory_lease: OutputMemoryLease,
    ) -> Self {
        Self { document, markdown, assets, diagnostics, provenance, memory_lease }
    }

    /// Reassemble a validated recovery payload while keeping the checkpoint
    /// decode reservation inseparable and certifying it against this exact
    /// result in the same execution context.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_recovered_accounted_parts(
        document: Document,
        markdown: String,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<Provenance>,
        context: &ExecutionContext,
        mut memory: ResourceReservation,
    ) -> Result<Self, ConversionError> {
        let retained =
            estimate_retained_result(&document, &markdown, &assets, &diagnostics, &provenance)?;
        certify_recovered_reservation(context, &mut memory, retained)?;
        let mut memory_lease = OutputMemoryLease::from_reservation(memory)?;
        memory_lease.accounted_bytes = retained;
        Ok(Self::from_accounted_parts(
            document,
            markdown,
            assets,
            diagnostics,
            provenance,
            memory_lease,
        ))
    }

    /// Whether this result retains a live request-memory charge.
    #[doc(hidden)]
    #[must_use]
    pub fn has_memory_lease(&self) -> bool {
        !self.memory_lease.leases.is_empty()
    }
}

/// Resolve one source class into bounded in-memory bytes.
pub trait SourceResolver: Send + Sync {
    /// Stable implementation ID.
    fn id(&self) -> &'static str;
    /// Whether this resolver handles the source shape.
    fn supports(&self, input: &InputRef) -> bool;
    /// Resolve the source while enforcing request policy.
    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>>;

    /// Resolve while optionally retaining request memory accounting across the
    /// resolver-to-engine handoff.
    ///
    /// Existing implementations inherit this allocation-free adapter. A
    /// resolver that reserves before constructing source bytes may override it
    /// and return the reservation with [`ResolvedSource`].
    fn resolve_accounted<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        Box::pin(
            async move { self.resolve(input, options, context).await.map(ResolvedSource::new) },
        )
    }
}

/// Produce format hypotheses from bytes, metadata, and explicit hints.
pub trait FormatDetector: Send + Sync {
    /// Stable implementation ID.
    fn id(&self) -> &'static str;
    /// Detector priority; larger values run first.
    fn priority(&self) -> i32 {
        0
    }
    /// Detect zero or more candidates.
    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>>;
}

/// Result of a cheap converter applicability probe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeOutcome {
    /// The converter does not apply; registry fallback may continue.
    NotApplicable,
    /// The converter applies with the supplied confidence.
    Match {
        /// Converter-specific confidence in the inclusive range `0.0..=1.0`.
        confidence: f32,
    },
}

/// Output produced by a format converter before Markdown rendering.
#[derive(Debug, Default)]
pub struct ConverterOutput {
    /// Unified document IR.
    pub document: Document,
    /// Extracted assets.
    pub assets: Vec<Asset>,
    /// Recoveries and scoped failures.
    pub diagnostics: Vec<Diagnostic>,
    /// Live memory charges transferred with the output until the engine has assembled its result.
    memory_lease: OutputMemoryLease,
}

impl ConverterOutput {
    /// Construct an output without transferred charges.
    #[must_use]
    pub fn new(document: Document, assets: Vec<Asset>, diagnostics: Vec<Diagnostic>) -> Self {
        Self { document, assets, diagnostics, memory_lease: OutputMemoryLease::default() }
    }

    /// Construct converter output with reservations attached exactly once.
    /// The ownership cannot subsequently be replaced or detached.
    #[doc(hidden)]
    #[must_use]
    pub fn new_with_memory_reservations(
        document: Document,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        leases: Vec<ResourceReservation>,
    ) -> Self {
        Self {
            document,
            assets,
            diagnostics,
            memory_lease: OutputMemoryLease::from_reservations(leases),
        }
    }

    /// Construct recovered/intermediate output with one already-held decode
    /// reservation without an infallible temporary reservation vector.
    #[doc(hidden)]
    pub fn new_with_memory_reservation(
        document: Document,
        assets: Vec<Asset>,
        diagnostics: Vec<Diagnostic>,
        context: &ExecutionContext,
        mut lease: ResourceReservation,
    ) -> Result<Self, ConversionError> {
        let retained = estimate_retained_output(&document, &assets, &diagnostics)?;
        certify_recovered_reservation(context, &mut lease, retained)?;
        let mut memory_lease = OutputMemoryLease::from_reservation(lease)?;
        memory_lease.accounted_bytes = retained;
        Ok(Self { document, assets, diagnostics, memory_lease })
    }

    /// Bind live reservations to this output using the central retained-size
    /// authority. Uncertified arbitrary reservations never offset engine
    /// accounting.
    #[doc(hidden)]
    pub fn account_retained(mut self, context: &ExecutionContext) -> Result<Self, ConversionError> {
        let required = estimate_retained_output(&self.document, &self.assets, &self.diagnostics)?;
        let held = self
            .memory_lease
            .leases
            .iter()
            .filter(|lease| lease.belongs_to_memory_context(context))
            .map(ResourceReservation::bytes)
            .fold(0_u64, u64::saturating_add);
        self.memory_lease.push(context.reserve_memory(required.saturating_sub(held))?)?;
        self.memory_lease.accounted_bytes = required;
        Ok(self)
    }

    /// Certify a reservation acquired before invoking an external converter,
    /// shrink it to the retained deficit, and attach it without an accounting gap.
    #[doc(hidden)]
    pub fn certify_preflight_reservation(
        mut self,
        context: &ExecutionContext,
        mut reservation: ResourceReservation,
    ) -> Result<Self, ConversionError> {
        let required = estimate_retained_output(&self.document, &self.assets, &self.diagnostics)?;
        if !reservation.belongs_to_memory_context(context) {
            return Err(ConversionError::Internal {
                detail: "converter preflight reservation belongs to a different context".into(),
            });
        }
        if reservation.bytes() < required {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!(
                    "converter retained {required} bytes beyond its {}-byte preflight plan",
                    reservation.bytes()
                ),
            });
        }
        // The outer reservation was held before converter entry and covers the
        // complete allocation peak. Drop every converter-supplied child lease
        // while that parent is still fully charged, then make the authenticated
        // parent the sole retained owner. If a provider leaked a child outside
        // this output, `shrink` rejects the still-active credit and the backing
        // remains globally charged until that child is destroyed.
        self.memory_lease = OutputMemoryLease::default();
        reservation.shrink(reservation.bytes().saturating_sub(required))?;
        self.memory_lease = OutputMemoryLease::from_reservation(reservation)?;
        self.memory_lease.accounted_bytes = required;
        Ok(self)
    }

    /// Consume the intermediate output and assemble a final result without
    /// exposing its detachable memory ownership.
    #[doc(hidden)]
    pub fn into_conversion_result(
        self,
        markdown: String,
        provenance: Vec<Provenance>,
        reservations: [Option<ResourceReservation>; 3],
    ) -> Result<ConversionResult, ConversionError> {
        let Self { document, assets, diagnostics, mut memory_lease } = self;
        for reservation in reservations.into_iter().flatten() {
            memory_lease.push(reservation)?;
        }
        Ok(ConversionResult::from_accounted_parts(
            document,
            markdown,
            assets,
            diagnostics,
            provenance,
            memory_lease,
        ))
    }

    #[doc(hidden)]
    #[must_use]
    pub fn leased_memory_for(&self, context: &ExecutionContext) -> u64 {
        self.memory_lease.bytes_for(context)
    }

    /// Transfer opaque retained-memory ownership from a consumed nested output
    /// without exposing detachable reservations to converter implementations.
    #[doc(hidden)]
    pub fn absorb_memory_lease(
        &mut self,
        source: &mut Self,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        if self
            .memory_lease
            .leases
            .iter()
            .chain(&source.memory_lease.leases)
            .any(|lease| !lease.belongs_to_memory_context(context))
        {
            return Err(ConversionError::Internal {
                detail: "nested output lease belongs to a different context".into(),
            });
        }
        let combined = self
            .memory_lease
            .accounted_bytes
            .checked_add(source.memory_lease.accounted_bytes)
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "nested output lease accounting overflowed".into(),
            })?;
        self.memory_lease.leases.try_reserve(source.memory_lease.leases.len()).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "nested output lease inventory allocation failed".into(),
            }
        })?;
        self.memory_lease.leases.append(&mut source.memory_lease.leases);
        self.memory_lease.accounted_bytes = combined;
        source.memory_lease.accounted_bytes = 0;
        Ok(())
    }

    /// Attach one request-owned reservation to an output being assembled by a
    /// recursive converter.
    #[doc(hidden)]
    pub fn attach_memory_reservation(
        &mut self,
        context: &ExecutionContext,
        reservation: ResourceReservation,
    ) -> Result<(), ConversionError> {
        if !reservation.belongs_to_memory_context(context) {
            return Err(ConversionError::Internal {
                detail: "nested output reservation belongs to a different context".into(),
            });
        }
        let combined =
            self.memory_lease.accounted_bytes.checked_add(reservation.bytes()).ok_or_else(
                || ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "nested output reservation accounting overflowed".into(),
                },
            )?;
        self.memory_lease.push(reservation)?;
        self.memory_lease.accounted_bytes = combined;
        Ok(())
    }
}

fn certify_recovered_reservation(
    context: &ExecutionContext,
    reservation: &mut ResourceReservation,
    required: u64,
) -> Result<(), ConversionError> {
    if !reservation.belongs_to_memory_context(context) {
        return Err(ConversionError::Internal {
            detail: "recovery memory reservation belongs to a different context".into(),
        });
    }
    match reservation.bytes().cmp(&required) {
        std::cmp::Ordering::Less => reservation.grow(required - reservation.bytes())?,
        std::cmp::Ordering::Greater => reservation.shrink(reservation.bytes() - required)?,
        std::cmp::Ordering::Equal => {}
    }
    Ok(())
}

/// Conservatively estimate bytes retained by a converter output and by the
/// output's directly owned heap graph.
///
/// The walk covers every owned heap allocation by capacity, including spare
/// capacity invisible in serialized wire data. The engine accounts its later
/// concrete provenance inventory separately when that allocation is made.
#[doc(hidden)]
pub fn estimate_retained_output(
    document: &Document,
    assets: &Vec<Asset>,
    diagnostics: &Vec<Diagnostic>,
) -> Result<u64, ConversionError> {
    let mut total = RetainedCounter::default();
    total.vec::<BlockNode>(document.blocks.capacity())?;
    total.string_opt(document.metadata.title.as_ref())?;
    total.vec::<String>(document.metadata.authors.capacity())?;
    for author in &document.metadata.authors {
        total.string(author)?;
    }
    total.btree_entries::<String, String>(document.metadata.properties.len())?;
    for (key, value) in &document.metadata.properties {
        total.string(key)?;
        total.string(value)?;
    }
    retained_blocks(&document.blocks, &mut total, false)?;
    total.vec::<Asset>(assets.capacity())?;
    for asset in assets {
        total.string(&asset.id.0)?;
        total.string_opt(asset.filename.as_ref())?;
        total.string(&asset.media_type)?;
        total.vec::<u8>(asset.bytes.capacity())?;
        total.string_opt(asset.external_uri.as_ref())?;
    }
    total.vec::<Diagnostic>(diagnostics.capacity())?;
    for diagnostic in diagnostics {
        total.string(&diagnostic.code)?;
        total.string(&diagnostic.message)?;
        if let Some(locator) = &diagnostic.locator {
            retained_locator(locator, &mut total)?;
        }
    }
    // Root structs and allocator/container bookkeeping margin.
    total.add(4_096)?;
    Ok(total.0)
}

/// Conservatively measure a complete final result, including Markdown and the
/// concrete provenance inventory capacity.
#[doc(hidden)]
pub fn estimate_retained_result(
    document: &Document,
    markdown: &String,
    assets: &Vec<Asset>,
    diagnostics: &Vec<Diagnostic>,
    provenance: &Vec<Provenance>,
) -> Result<u64, ConversionError> {
    let mut total = estimate_retained_output(document, assets, diagnostics)?;
    total = total
        .checked_add(u64::try_from(markdown.capacity()).map_err(|_| memory_estimate_overflow())?)
        .ok_or_else(memory_estimate_overflow)?;
    let inventory = size_of::<Provenance>()
        .checked_mul(provenance.capacity())
        .ok_or_else(memory_estimate_overflow)?;
    total = total
        .checked_add(u64::try_from(inventory).map_err(|_| memory_estimate_overflow())?)
        .ok_or_else(memory_estimate_overflow)?;
    let mut owned = RetainedCounter(total);
    for item in provenance {
        retained_provenance(item, &mut owned)?;
    }
    Ok(owned.0)
}

/// Conservatively plan the temporary heap used by typed IR, asset, and
/// diagnostic validation. The walk itself is allocation-free and stops at the
/// public depth limit, so an over-deep untrusted tree cannot recurse without a
/// bound before the engine acquires this reservation.
#[doc(hidden)]
#[allow(clippy::too_many_lines)]
pub fn estimate_validation_working_set(
    document: &Document,
    assets: &[Asset],
    diagnostics: &[Diagnostic],
) -> Result<u64, ConversionError> {
    const PATH_AND_TREE_NODE_HIGH_WATER: usize = 4_096;
    const INLINE_HIGH_WATER: usize = 2_048;
    const ASSET_DIAGNOSTIC_HIGH_WATER: usize = 4_096;

    fn add(total: &mut usize, bytes: usize) -> Result<(), ConversionError> {
        *total = total.checked_add(bytes).ok_or_else(memory_estimate_overflow)?;
        Ok(())
    }

    fn strings_in_provenance(value: &Provenance) -> Result<usize, ConversionError> {
        [
            Some(&value.provider),
            value.locator.sheet.as_ref(),
            value.locator.font_name.as_ref(),
            value.locator.part.as_ref(),
        ]
        .into_iter()
        .flatten()
        .try_fold(0_usize, |total, value| {
            total.checked_add(value.len()).ok_or_else(memory_estimate_overflow)
        })
    }

    fn visit_inlines(
        values: &[crate::Inline],
        depth: usize,
        total: &mut usize,
        inline_count: &mut usize,
    ) -> Result<(), ConversionError> {
        if depth > 2 {
            // Typed validation rejects a nested link before following its own
            // content. Mirror that bounded control flow so an adversarial
            // already-constructed chain cannot recurse this pre-permit walk.
            return Err(ConversionError::Internal {
                detail: "validation preflight rejected documentDepth: nested inline link".into(),
            });
        }
        for value in values {
            *inline_count = inline_count.saturating_add(1);
            if *inline_count > crate::MAX_DOCUMENT_INLINES {
                return Err(ConversionError::ResourceLimit {
                    limit: "documentInlines",
                    detail: format!("{} > {}", *inline_count, crate::MAX_DOCUMENT_INLINES),
                });
            }
            add(total, INLINE_HIGH_WATER)?;
            match value {
                crate::Inline::SourceText { provenance, .. } => {
                    add(total, strings_in_provenance(provenance)?)?;
                }
                crate::Inline::OcrText { provenance, evidence, .. } => {
                    add(total, strings_in_provenance(provenance)?)?;
                    add(total, evidence.regions.len().saturating_mul(512))?;
                    add(total, evidence.chain.len().saturating_mul(512))?;
                    for step in &evidence.chain {
                        add(total, step.provider.len())?;
                        if let Some(model) = &step.model {
                            add(total, model.len())?;
                        }
                    }
                }
                crate::Inline::Link { content, .. } => {
                    visit_inlines(content, depth + 1, total, inline_count)?;
                }
                crate::Inline::FootnoteReference(label) => {
                    // The validator clones this into its reference B-tree.
                    add(total, label.len())?;
                    add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn visit_nodes(
        nodes: &[BlockNode],
        depth: usize,
        total: &mut usize,
        maximum_table_width: &mut usize,
        node_count: &mut usize,
        inline_count: &mut usize,
    ) -> Result<(), ConversionError> {
        if depth > crate::MAX_DOCUMENT_DEPTH {
            return Err(ConversionError::Internal {
                detail: format!(
                    "validation preflight rejected documentDepth: {depth} > {}",
                    crate::MAX_DOCUMENT_DEPTH
                ),
            });
        }
        for node in nodes {
            *node_count = node_count.saturating_add(1);
            if *node_count > crate::MAX_DOCUMENT_NODES {
                return Err(ConversionError::ResourceLimit {
                    limit: "documentNodes",
                    detail: format!("{} > {}", *node_count, crate::MAX_DOCUMENT_NODES),
                });
            }
            add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
            add(total, node.id.0.len())?; // cloned into the node-ID B-tree
            add(total, strings_in_provenance(&node.provenance)?)?;
            match &node.block {
                crate::Block::Paragraph(values)
                | crate::Block::Heading { content: values, .. }
                | crate::Block::TimedSegment { content: values, .. } => {
                    visit_inlines(values, 1, total, inline_count)?;
                }
                crate::Block::List { items, .. } => {
                    for item in items {
                        add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
                        visit_nodes(
                            &item.blocks,
                            depth + 1,
                            total,
                            maximum_table_width,
                            node_count,
                            inline_count,
                        )?;
                    }
                }
                crate::Block::Table { rows, .. } => {
                    for row in rows {
                        add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
                        let width = row.cells.iter().try_fold(0_usize, |width, cell| {
                            width
                                .checked_add(
                                    usize::try_from(cell.column_span).unwrap_or(usize::MAX),
                                )
                                .ok_or_else(memory_estimate_overflow)
                        })?;
                        *maximum_table_width = (*maximum_table_width).max(width);
                        for cell in &row.cells {
                            add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
                            visit_nodes(
                                &cell.blocks,
                                depth + 1,
                                total,
                                maximum_table_width,
                                node_count,
                                inline_count,
                            )?;
                        }
                    }
                }
                crate::Block::Footnote { label, blocks } => {
                    add(total, label.len())?; // cloned into the definition B-tree
                    add(total, PATH_AND_TREE_NODE_HIGH_WATER)?;
                    visit_nodes(
                        blocks,
                        depth + 1,
                        total,
                        maximum_table_width,
                        node_count,
                        inline_count,
                    )?;
                }
                crate::Block::Page { blocks, .. }
                | crate::Block::Slide { blocks, .. }
                | crate::Block::Sheet { blocks, .. } => {
                    visit_nodes(
                        blocks,
                        depth + 1,
                        total,
                        maximum_table_width,
                        node_count,
                        inline_count,
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    let mut bytes = 4_096_usize;
    let mut maximum_table_width = 0_usize;
    let (mut node_count, mut inline_count) = (0_usize, 0_usize);
    visit_nodes(
        &document.blocks,
        1,
        &mut bytes,
        &mut maximum_table_width,
        &mut node_count,
        &mut inline_count,
    )?;
    add(
        &mut bytes,
        maximum_table_width
            .min(crate::MAX_TABLE_COLUMNS)
            .checked_mul(size_of::<u32>())
            .ok_or_else(memory_estimate_overflow)?,
    )?;
    for asset in assets {
        add(&mut bytes, ASSET_DIAGNOSTIC_HIGH_WATER)?;
        for value in [
            Some(&asset.id.0),
            asset.filename.as_ref(),
            Some(&asset.media_type),
            asset.external_uri.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            // Covers borrowed B-tree nodes plus canonical-URI parser scratch.
            add(&mut bytes, value.len().saturating_mul(4))?;
        }
    }
    for diagnostic in diagnostics {
        add(&mut bytes, ASSET_DIAGNOSTIC_HIGH_WATER)?;
        add(&mut bytes, diagnostic.code.len())?;
        add(&mut bytes, diagnostic.message.len())?;
        if let Some(locator) = &diagnostic.locator {
            for value in [locator.sheet.as_ref(), locator.font_name.as_ref(), locator.part.as_ref()]
                .into_iter()
                .flatten()
            {
                add(&mut bytes, value.len())?;
            }
        }
    }
    u64::try_from(bytes).map_err(|_| memory_estimate_overflow())
}

#[derive(Default)]
struct RetainedCounter(u64);

impl RetainedCounter {
    fn add(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.0 = self
            .0
            .checked_add(u64::try_from(bytes).map_err(|_| memory_estimate_overflow())?)
            .ok_or_else(memory_estimate_overflow)?;
        Ok(())
    }

    fn vec<T>(&mut self, capacity: usize) -> Result<(), ConversionError> {
        self.add(size_of::<T>().checked_mul(capacity).ok_or_else(memory_estimate_overflow)?)
    }

    fn string(&mut self, value: &String) -> Result<(), ConversionError> {
        self.add(value.capacity())
    }

    fn string_opt(&mut self, value: Option<&String>) -> Result<(), ConversionError> {
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn btree_entries<K, V>(&mut self, len: usize) -> Result<(), ConversionError> {
        const NODE_SLOTS: usize = 11;
        const EDGE_SLOTS: usize = 12;
        const NODE_BOOKKEEPING: usize = 256;
        if len == 0 {
            return Ok(());
        }
        // Rust's BTreeMap nodes reserve multiple key/value and edge slots even
        // when sparsely occupied. Charging one complete high-water node per
        // live entry deliberately over-approximates leaf and internal nodes,
        // including parent/length metadata, alignment, and allocator headers.
        let width = size_of::<K>()
            .checked_add(size_of::<V>())
            .and_then(|value| value.checked_mul(NODE_SLOTS))
            .and_then(|value| value.checked_add(size_of::<usize>() * EDGE_SLOTS))
            .and_then(|value| value.checked_add(NODE_BOOKKEEPING))
            .ok_or_else(memory_estimate_overflow)?;
        self.add(width.checked_mul(len).ok_or_else(memory_estimate_overflow)?)
    }
}

fn retained_blocks(
    blocks: &Vec<BlockNode>,
    total: &mut RetainedCounter,
    include_provenance_clone: bool,
) -> Result<(), ConversionError> {
    for node in blocks {
        total.string(&node.id.0)?;
        retained_provenance(&node.provenance, total)?;
        if include_provenance_clone {
            // The engine preflights an exact count, but allocators may return a
            // larger capacity. Two element slots per record bounds Vec growth.
            total.add(
                size_of::<Provenance>().checked_mul(2).ok_or_else(memory_estimate_overflow)?,
            )?;
            retained_provenance(&node.provenance, total)?;
        }
        retained_block(&node.block, total, include_provenance_clone)?;
    }
    Ok(())
}

fn retained_block(
    block: &crate::Block,
    total: &mut RetainedCounter,
    clone_provenance: bool,
) -> Result<(), ConversionError> {
    use crate::Block;
    match block {
        Block::Paragraph(values) | Block::Heading { content: values, .. } => {
            retained_inlines(values, total)?;
        }
        Block::List { items, .. } => {
            total.vec::<crate::ListItem>(items.capacity())?;
            for item in items {
                total.string_opt(item.marker_label.as_ref())?;
                total.vec::<BlockNode>(item.blocks.capacity())?;
                retained_blocks(&item.blocks, total, clone_provenance)?;
            }
        }
        Block::Table { rows, alignments } => {
            total.vec::<crate::TableRow>(rows.capacity())?;
            total.vec::<crate::TableAlignment>(alignments.capacity())?;
            for row in rows {
                total.vec::<crate::Cell>(row.cells.capacity())?;
                for cell in &row.cells {
                    total.vec::<BlockNode>(cell.blocks.capacity())?;
                    retained_blocks(&cell.blocks, total, clone_provenance)?;
                }
            }
        }
        Block::Code { language, text } => {
            total.string_opt(language.as_ref())?;
            total.string(text)?;
        }
        Block::Formula(value) => total.string(value)?,
        Block::Footnote { label, blocks } => {
            total.string(label)?;
            total.vec::<BlockNode>(blocks.capacity())?;
            retained_blocks(blocks, total, clone_provenance)?;
        }
        Block::Image { asset, alt } => {
            total.string(&asset.0)?;
            total.string_opt(alt.as_ref())?;
        }
        Block::Page { blocks, .. } => {
            total.vec::<BlockNode>(blocks.capacity())?;
            retained_blocks(blocks, total, clone_provenance)?;
        }
        Block::Slide { title, blocks, .. } => {
            total.string_opt(title.as_ref())?;
            total.vec::<BlockNode>(blocks.capacity())?;
            retained_blocks(blocks, total, clone_provenance)?;
        }
        Block::Sheet { name, blocks } => {
            total.string(name)?;
            total.vec::<BlockNode>(blocks.capacity())?;
            retained_blocks(blocks, total, clone_provenance)?;
        }
        Block::TimedSegment { speaker, content, .. } => {
            total.string_opt(speaker.as_ref())?;
            retained_inlines(content, total)?;
        }
        Block::Rule => {}
    }
    Ok(())
}

fn retained_inlines(
    values: &Vec<crate::Inline>,
    total: &mut RetainedCounter,
) -> Result<(), ConversionError> {
    use crate::Inline;
    total.vec::<Inline>(values.capacity())?;
    for value in values {
        match value {
            Inline::Text { value, marks } => {
                total.string(value)?;
                total.vec::<crate::InlineMark>(marks.capacity())?;
            }
            Inline::SourceText { value, marks, provenance } => {
                total.string(value)?;
                total.vec::<crate::InlineMark>(marks.capacity())?;
                total.add(size_of::<Provenance>())?;
                retained_provenance(provenance, total)?;
            }
            Inline::OcrText { value, marks, provenance, evidence } => {
                total.string(value)?;
                total.vec::<crate::InlineMark>(marks.capacity())?;
                total.add(size_of::<Provenance>())?;
                retained_provenance(provenance, total)?;
                total.add(size_of::<crate::OcrEvidence>())?;
                total.vec::<crate::OcrSourceRegion>(evidence.regions.capacity())?;
                total.vec::<crate::OcrEvidenceStep>(evidence.chain.capacity())?;
                for step in &evidence.chain {
                    total.string(&step.provider)?;
                    total.string_opt(step.model.as_ref())?;
                }
            }
            Inline::Code(value) | Inline::Formula(value) | Inline::FootnoteReference(value) => {
                total.string(value)?;
            }
            Inline::Link { target, content } => {
                total.string(target)?;
                retained_inlines(content, total)?;
            }
            Inline::LineBreak => {}
        }
    }
    Ok(())
}

fn retained_provenance(
    provenance: &Provenance,
    total: &mut RetainedCounter,
) -> Result<(), ConversionError> {
    total.string(&provenance.provider)?;
    retained_locator(&provenance.locator, total)
}

fn retained_locator(
    locator: &crate::SourceLocator,
    total: &mut RetainedCounter,
) -> Result<(), ConversionError> {
    total.string_opt(locator.sheet.as_ref())?;
    total.string_opt(locator.font_name.as_ref())?;
    total.string_opt(locator.part.as_ref())
}

fn memory_estimate_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "retained output memory estimate overflowed".into(),
    }
}

/// Parse one or more source formats into the unified IR.
pub trait Converter: Send + Sync {
    /// Stable implementation ID, also used as deterministic tie breaker.
    fn id(&self) -> &'static str;
    /// Registry priority; larger values are attempted first after confidence.
    fn priority(&self) -> i32 {
        0
    }
    /// Formats implemented by this converter.
    fn supported_formats(&self) -> &'static [InputFormat];
    /// Cheap applicability check that must not perform full conversion.
    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>>;
    /// Conservative live-allocation peak required for conversion, returned
    /// output, and the engine's bounded IR validation working set. The engine
    /// holds this reservation through validation and only then certifies and
    /// shrinks it to the retained output.
    ///
    /// # Errors
    ///
    /// Returns a stable component or resource error when no safe plan can be
    /// declared for this request.
    fn planned_output_bytes(
        &self,
        input: &ResolvedInput,
        candidate: &FormatCandidate,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        let _ = (input, candidate, options, context);
        Err(ConversionError::ComponentUnavailable {
            component: self.id().into(),
            detail: "converter does not declare a preflight memory plan".into(),
        })
    }
    /// Convert a confirmed input. Any error is authoritative and stops
    /// fallback; only `ProbeOutcome::NotApplicable` permits the next attempt.
    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>>;
}

/// Render the unified IR through a single Markdown policy.
pub trait MarkdownRenderer: Send + Sync {
    /// Stable renderer ID.
    fn id(&self) -> &'static str;
    /// Conservative Markdown allocation peak reserved before rendering begins.
    ///
    /// # Errors
    ///
    /// Returns a stable component or resource error when no safe plan can be
    /// declared for this document and asset inventory.
    fn planned_markdown_bytes(
        &self,
        document: &Document,
        assets: &[Asset],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        let _ = (document, assets, options, context);
        Err(ConversionError::ComponentUnavailable {
            component: self.id().into(),
            detail: "renderer does not declare a preflight memory plan".into(),
        })
    }
    /// Render a document and its asset inventory.
    fn render<'a>(
        &'a self,
        document: &'a Document,
        assets: &'a [Asset],
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<String, ConversionError>>;
}

/// OCR request over one decoded image.
#[derive(Debug, Clone, Copy)]
pub struct OcrRequest<'a> {
    /// Encoded image bytes.
    pub image: &'a [u8],
    /// MIME media type.
    pub media_type: &'a str,
    /// Optional language hints such as `zh-Hans`, `zh-Hant`, and `en`.
    pub languages: &'a [&'a str],
}

/// Native or remote OCR implementation.
pub trait OcrEngine: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Recognize text and geometry.
    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>>;
    /// Recognize text with additive, identity-bound structured evidence.
    ///
    /// Existing providers remain source compatible and explicitly produce an
    /// unbound result. A consumer that emits structured OCR provenance must
    /// reject or visibly degrade [`OcrRecognition::Unbound`]; it must never
    /// invent detector confidence or model identity.
    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move { self.recognize(request, context).await.map(OcrRecognition::Unbound) })
    }
}

/// Audio transcription request.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptionRequest<'a> {
    /// Encoded media bytes.
    pub media: &'a [u8],
    /// MIME media type.
    pub media_type: &'a str,
    /// Optional BCP-47 language hint.
    pub language: Option<&'a str>,
}

/// Time-aligned transcription result represented as IR nodes.
#[derive(Debug, Clone, Default)]
pub struct TranscriptionResult {
    /// Ordered timed segment nodes.
    pub segments: Vec<BlockNode>,
    /// Provider/model ID.
    pub provider: String,
}

/// Local or remote speech-to-text provider.
pub trait Transcriber: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Transcribe media.
    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>>;
}

/// Optional AI operation exposed by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AiCapability {
    /// Vision OCR.
    VisionOcr,
    /// Image description.
    ImageDescription,
    /// Layout repair.
    LayoutRepair,
    /// Table repair.
    TableRepair,
    /// Formula repair.
    FormulaRepair,
    /// Audio transcription.
    AudioTranscription,
    /// Markdown post-processing.
    MarkdownPostprocess,
}

/// Borrowed input to an AI operation.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum AiInput<'a> {
    /// Encoded image.
    Image {
        /// Encoded image bytes.
        bytes: &'a [u8],
        /// MIME media type.
        media_type: &'a str,
    },
    /// Structured document.
    Document(&'a Document),
    /// Markdown text.
    Markdown(&'a str),
    /// Encoded audio/video media.
    Media {
        /// Encoded media bytes.
        bytes: &'a [u8],
        /// MIME media type.
        media_type: &'a str,
    },
}

/// AI operation request.
#[derive(Debug, Clone, Copy)]
pub struct AiRequest<'a> {
    /// Required capability.
    pub capability: AiCapability,
    /// Typed input.
    pub input: AiInput<'a>,
    /// Optional user-controlled prompt suffix.
    pub prompt: Option<&'a str>,
}

/// Versioned, validated changes an AI provider may propose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentPatch {
    /// Protocol version. The initial contract is `1`.
    pub version: u32,
    /// Ordered patch operations.
    pub operations: Vec<PatchOperation>,
}

/// Allowed structured IR edits. Raw provider-specific mutation is forbidden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PatchOperation {
    /// Append new nodes at the document root.
    Append {
        /// Nodes to append at document root.
        nodes: Vec<BlockNode>,
    },
    /// Replace one node while retaining an auditable target ID.
    Replace {
        /// Existing node ID to replace.
        target: crate::NodeId,
        /// Replacement nodes with AI provenance.
        nodes: Vec<BlockNode>,
    },
}

impl DocumentPatch {
    /// Validate the wire version and basic structural invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Ai`] when the patch protocol version or an
    /// operation is not accepted by this library version.
    pub fn validate(&self) -> Result<(), ConversionError> {
        if self.version != 1 {
            return Err(ConversionError::Ai {
                provider: "patch-validator".into(),
                detail: format!("unsupported document patch version {}", self.version),
            });
        }
        Ok(())
    }
}

/// Structured output returned by an AI provider.
#[derive(Debug, Clone, Default)]
pub struct AiOutput {
    /// Provider-created nodes with AI provenance.
    pub nodes: Vec<BlockNode>,
    /// Optional structured patch.
    pub patch: Option<DocumentPatch>,
    /// Non-fatal diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

/// Capability-negotiated LLM or multimodal provider.
pub trait AiProvider: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Capabilities available under current configuration.
    fn capabilities(&self) -> BTreeSet<AiCapability>;
    /// Declare a conservative allocation peak for one policy-bound operation.
    ///
    /// The safe default refuses policy-bound execution. Existing providers keep
    /// their source compatibility, while callers never infer a bound for an
    /// implementation which has not declared one.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::ComponentUnavailable`] by default. An opted-in
    /// provider may also reject the request policy or resource plan.
    fn planned_output_bytes(
        &self,
        request: AiRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        let _ = (request, options, context);
        Err(ConversionError::ComponentUnavailable {
            component: self.id().into(),
            detail: "AI provider does not declare a policy-bound allocation plan".into(),
        })
    }
    /// Execute while receiving the complete request policy that authorized the operation.
    ///
    /// The safe default rejects the request instead of delegating to the legacy
    /// `execute` method, because that method cannot observe network or resource
    /// policy. Providers opt in only after auditing this boundary.
    fn execute_with_options<'a>(
        &'a self,
        request: AiRequest<'a>,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        let _ = (request, options);
        Box::pin(async move {
            context.checkpoint()?;
            Err(ConversionError::ComponentUnavailable {
                component: self.id().into(),
                detail: "AI provider does not implement policy-bound execution".into(),
            })
        })
    }
    /// Execute one explicitly enabled capability.
    fn execute<'a>(
        &'a self,
        request: AiRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>>;
}

/// Minimal tensor exchange type for inference runtimes.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    /// Row-major dimensions.
    pub shape: Vec<usize>,
    /// Float data. Quantized runtimes adapt at the provider boundary.
    pub values: Vec<f32>,
}

/// Model-runtime seam used by local OCR without coupling the OCR API to ORT.
pub trait TensorRuntime: Send + Sync {
    /// Stable runtime ID.
    fn id(&self) -> &'static str;
    /// Execute a named model with ordered input tensors.
    fn run<'a>(
        &'a self,
        model_id: &'a str,
        inputs: &'a [Tensor],
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>>;
}

/// Optional services made available to converters.
#[derive(Clone, Default)]
pub struct Services {
    /// OCR implementation.
    pub ocr: Option<Arc<dyn OcrEngine>>,
    /// Speech transcription implementation.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    /// AI provider.
    pub ai: Option<Arc<dyn AiProvider>>,
    /// Request-authority-preserving dispatcher for already-resolved container members.
    pub nested: Option<Arc<dyn crate::NestedConversionService>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe() {
        let _: Option<&dyn SourceResolver> = None;
        let _: Option<&dyn FormatDetector> = None;
        let _: Option<&dyn Converter> = None;
        let _: Option<&dyn MarkdownRenderer> = None;
        let _: Option<&dyn OcrEngine> = None;
        let _: Option<&dyn Transcriber> = None;
        let _: Option<&dyn AiProvider> = None;
        let _: Option<&dyn TensorRuntime> = None;
    }

    #[test]
    fn service_provider_interfaces_are_object_safe() {
        assert_object_safe();
    }

    #[test]
    fn document_patch_rejects_unknown_versions() {
        let patch = DocumentPatch { version: 2, operations: vec![] };
        assert_eq!(patch.validate().unwrap_err().code(), crate::ErrorCode::Ai);
    }

    #[test]
    fn retained_estimate_counts_spare_capacities_and_exact_lifetime_charge() {
        fn fixture() -> (Document, Vec<Asset>, Vec<Diagnostic>) {
            let mut title = String::with_capacity(16_384);
            title.push('x');
            let mut bytes = Vec::with_capacity(32_768);
            bytes.push(1);
            let document = Document {
                metadata: crate::DocumentMetadata { title: Some(title), ..Default::default() },
                ..Default::default()
            };
            let assets = vec![Asset {
                id: crate::AssetId("a".into()),
                filename: None,
                media_type: "application/octet-stream".into(),
                bytes,
                external_uri: None,
            }];
            (document, assets, Vec::new())
        }
        let (document, assets, diagnostics) = fixture();
        let required = estimate_retained_output(&document, &assets, &diagnostics).unwrap();
        assert!(required >= 16_384 + 32_768);

        let (low_document, low_assets, low_diagnostics) = fixture();
        let low_required =
            estimate_retained_output(&low_document, &low_assets, &low_diagnostics).unwrap();
        let low = ExecutionContext::new(
            ExecutionOptions::default(),
            crate::ResourceLimits {
                max_memory_bytes: low_required - 1,
                ..crate::ResourceLimits::default()
            },
        );
        assert!(matches!(
            ConverterOutput::new(low_document, low_assets, low_diagnostics).account_retained(&low),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(low.reserved_memory_bytes(), 0);

        let exact = ExecutionContext::new(
            ExecutionOptions::default(),
            crate::ResourceLimits {
                max_memory_bytes: required,
                ..crate::ResourceLimits::default()
            },
        );
        let output =
            ConverterOutput::new(document, assets, diagnostics).account_retained(&exact).unwrap();
        assert_eq!(exact.reserved_memory_bytes(), required);
        let result =
            output.into_conversion_result(String::new(), Vec::new(), [None, None, None]).unwrap();
        assert_eq!(exact.reserved_memory_bytes(), required);
        drop(result);
        assert_eq!(exact.reserved_memory_bytes(), 0);
    }

    #[test]
    fn nested_output_lease_transfer_rejects_a_different_context() {
        let limits = crate::ResourceLimits::default();
        let expected = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let foreign = ExecutionContext::new(ExecutionOptions::default(), limits);
        let reservation = foreign.reserve_memory(4_096).unwrap();
        let mut source = ConverterOutput::new_with_memory_reservation(
            Document::default(),
            Vec::new(),
            Vec::new(),
            &foreign,
            reservation,
        )
        .unwrap();
        let mut target = ConverterOutput::default();
        assert!(matches!(
            target.absorb_memory_lease(&mut source, &expected),
            Err(ConversionError::Internal { .. })
        ));
        assert!(source.leased_memory_for(&foreign) >= 4_096);
    }

    #[test]
    fn recovered_result_certifies_and_credits_same_context_decode_memory() {
        let document = Document {
            metadata: crate::DocumentMetadata {
                title: Some("recovered".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let markdown = String::from("recovered\n");
        let required =
            estimate_retained_result(&document, &markdown, &Vec::new(), &Vec::new(), &Vec::new())
                .unwrap();
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            crate::ResourceLimits { max_memory_bytes: required, ..Default::default() },
        );
        let decoded = context.reserve_memory(required / 2).unwrap();
        let result = ConversionResult::from_recovered_accounted_parts(
            document,
            markdown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &context,
            decoded,
        )
        .unwrap();
        assert_eq!(context.reserved_memory_bytes(), required);
        drop(result);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let document = Document {
            metadata: crate::DocumentMetadata {
                title: Some("recovered".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let markdown = String::from("recovered\n");
        let oversized_context = ExecutionContext::new(
            ExecutionOptions::default(),
            crate::ResourceLimits { max_memory_bytes: required * 3, ..Default::default() },
        );
        let decoded = oversized_context.reserve_memory(required * 2).unwrap();
        let result = ConversionResult::from_recovered_accounted_parts(
            document,
            markdown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &oversized_context,
            decoded,
        )
        .unwrap();
        assert_eq!(oversized_context.reserved_memory_bytes(), required);
        drop(result);
        assert_eq!(oversized_context.reserved_memory_bytes(), 0);

        let document = Document {
            metadata: crate::DocumentMetadata {
                title: Some("recovered".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let markdown = String::from("recovered\n");
        let low = ExecutionContext::new(
            ExecutionOptions::default(),
            crate::ResourceLimits { max_memory_bytes: required - 1, ..Default::default() },
        );
        let decoded = low.reserve_memory(required / 2).unwrap();
        assert!(matches!(
            ConversionResult::from_recovered_accounted_parts(
                document,
                markdown,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &low,
                decoded,
            ),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(low.reserved_memory_bytes(), 0);
    }

    #[test]
    fn retained_estimate_charges_sparse_btree_nodes_at_high_water() {
        let empty =
            estimate_retained_output(&Document::default(), &Vec::new(), &Vec::new()).unwrap();
        let mut document = Document::default();
        document.metadata.properties.insert("k".into(), "v".into());
        let one = estimate_retained_output(&document, &Vec::new(), &Vec::new()).unwrap();
        assert!(
            one.saturating_sub(empty)
                >= u64::try_from(11 * (size_of::<String>() * 2) + 256).unwrap()
        );

        for index in 0..32 {
            document.metadata.properties.insert(index.to_string(), String::new());
        }
        let many = estimate_retained_output(&document, &Vec::new(), &Vec::new()).unwrap();
        assert!(many > one);
    }

    #[test]
    fn validation_plan_bounds_overdeep_inline_without_recursing_unboundedly() {
        fn provenance() -> Provenance {
            Provenance {
                kind: crate::ProvenanceKind::NativeParser,
                provider: "test".into(),
                locator: crate::SourceLocator::default(),
                confidence: None,
            }
        }
        fn linked(depth: usize) -> crate::Inline {
            let mut value = crate::Inline::Text { value: "x".into(), marks: Vec::new() };
            for _ in 0..depth {
                value = crate::Inline::Link {
                    target: "https://example.test".into(),
                    content: vec![value],
                };
            }
            value
        }
        for (depth, valid) in [(1, true), (2_048, false)] {
            let document = Document {
                blocks: vec![BlockNode {
                    id: crate::NodeId("node".into()),
                    block: crate::Block::Paragraph(vec![linked(depth)]),
                    provenance: provenance(),
                }],
                ..Document::default()
            };
            assert_eq!(estimate_validation_working_set(&document, &[], &[]).is_ok(), valid);
            // Iteratively dismantle the adversarial chain so the test itself
            // does not recurse through enum drops.
            let mut value = document.blocks.into_iter().next().unwrap().block;
            while let crate::Block::Paragraph(mut values) = value {
                let Some(inline) = values.pop() else { break };
                match inline {
                    crate::Inline::Link { mut content, .. } => {
                        value = crate::Block::Paragraph(
                            content.pop().into_iter().collect::<Vec<crate::Inline>>(),
                        );
                    }
                    _ => break,
                }
            }
        }
    }

    #[test]
    fn final_retained_estimate_charges_exactly_one_provenance_inventory() {
        let mut provider = String::with_capacity(64 * 1024);
        provider.push('p');
        let provenance = Provenance {
            kind: crate::ProvenanceKind::NativeParser,
            provider,
            locator: crate::SourceLocator::default(),
            confidence: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: crate::NodeId("node".into()),
                block: crate::Block::Rule,
                provenance: Provenance {
                    kind: crate::ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: crate::SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        let inventory = vec![provenance];
        let output = estimate_retained_output(&document, &Vec::new(), &Vec::new()).unwrap();
        let final_bytes = estimate_retained_result(
            &document,
            &String::new(),
            &Vec::new(),
            &Vec::new(),
            &inventory,
        )
        .unwrap();
        let inventory_bytes = u64::try_from(
            inventory.capacity() * size_of::<Provenance>() + inventory[0].provider.capacity(),
        )
        .unwrap();
        assert_eq!(final_bytes, output + inventory_bytes);
        assert!(final_bytes < output + inventory_bytes * 2);
    }
}
