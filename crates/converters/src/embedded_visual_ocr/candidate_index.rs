use super::resource;
use into_markdown_core::{
    Asset, AssetId, ConversionError, ConverterOutput, ExecutionContext, ResourceReservation,
};
use std::collections::BTreeMap;

const REFERENCE_BASE_BYTES: u64 = 4 * 1024;
const BYTES_PER_REFERENCE_ENTRY: u64 = 256;
const CANDIDATE_BUFFER_BASE_BYTES: u64 = 1024;
const BYTES_PER_CANDIDATE_BUFFER_ENTRY: u64 = 128;
const GROUPING_BASE_BYTES: u64 = 4 * 1024;
const BYTES_PER_GROUPING_CANDIDATE: u64 = 1024;

#[cfg(test)]
thread_local! {
    static ASSET_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FULL_BYTE_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Preserve the converter invariant when the indexed validation lease cannot
/// be acquired. This failure-only path deliberately scans without allocating:
/// a dangling reference remains a hard error in document order, while a
/// complete document lets the caller return its original memory limit.
pub(super) fn validate_visual_references_without_index(
    output: &ConverterOutput,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    super::for_each_visual_reference(&output.document.blocks, context, &mut |asset_id| {
        for (index, asset) in output.assets.iter().enumerate() {
            if index.is_multiple_of(256) {
                context.checkpoint()?;
            }
            if asset.id == *asset_id {
                return Ok(());
            }
        }
        Err(ConversionError::Internal {
            detail: format!("image node references missing asset {}", asset_id.0),
        })
    })
}

pub(super) struct ReferenceIndex {
    entries: BTreeMap<AssetId, ReferenceEntry>,
    recorded: usize,
    recorded_id_bytes: u64,
    max_references: usize,
    max_id_bytes: u64,
    planned_bytes: u64,
    _memory: ResourceReservation,
}

struct ReferenceEntry {
    count: u64,
    present: bool,
    first_seen: usize,
}

impl ReferenceIndex {
    pub(super) fn new(
        max_references: usize,
        max_id_bytes: u64,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let planned_bytes = reference_index_plan(max_references, max_id_bytes)?;
        let memory = context.reserve_memory(planned_bytes)?;
        Ok(Self {
            entries: BTreeMap::new(),
            recorded: 0,
            recorded_id_bytes: 0,
            max_references,
            max_id_bytes,
            planned_bytes,
            _memory: memory,
        })
    }

    pub(super) fn record(&mut self, asset_id: &AssetId) -> Result<(), ConversionError> {
        let id_bytes = u64::try_from(asset_id.0.len()).map_err(|_| {
            resource("max_memory_bytes", "embedded visual reference ID length is not representable")
        })?;
        let next_recorded = self.recorded.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "embedded visual reference count is not representable")
        })?;
        let next_id_bytes = self.recorded_id_bytes.checked_add(id_bytes).ok_or_else(|| {
            resource("max_memory_bytes", "embedded visual reference ID bytes overflow")
        })?;
        if next_recorded > self.max_references || next_id_bytes > self.max_id_bytes {
            return Err(resource(
                "max_memory_bytes",
                "embedded visual reference inventory exceeded its preflight plan",
            ));
        }
        if let Some(entry) = self.entries.get_mut(asset_id) {
            entry.count = entry.count.checked_add(1).ok_or_else(|| {
                resource(
                    "max_archive_entries",
                    "embedded visual reference count is not representable",
                )
            })?;
        } else {
            self.entries.insert(
                asset_id.clone(),
                ReferenceEntry { count: 1, present: false, first_seen: self.recorded },
            );
        }
        self.recorded = next_recorded;
        self.recorded_id_bytes = next_id_bytes;
        Ok(())
    }

    pub(super) fn count(&self, asset_id: &AssetId) -> Option<u64> {
        self.entries.get(asset_id).map(|entry| entry.count)
    }

    pub(super) fn unique_len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn planned_bytes(&self) -> u64 {
        self.planned_bytes
    }

    /// Validate every referenced ID with one ordered-map lookup per asset.
    /// Repeated asset IDs preserve the existing presence semantics.
    pub(super) fn validate_assets(
        &mut self,
        assets: &[Asset],
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        for (index, asset) in assets.iter().enumerate() {
            if index.is_multiple_of(256) {
                context.checkpoint()?;
            }
            #[cfg(test)]
            ASSET_LOOKUPS.set(ASSET_LOOKUPS.get() + 1);
            if let Some(entry) = self.entries.get_mut(&asset.id) {
                entry.present = true;
            }
        }
        if let Some((asset_id, _)) = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.present)
            .min_by_key(|(_, entry)| entry.first_seen)
        {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", asset_id.0),
            });
        }
        Ok(())
    }
}

pub(super) fn reference_index_plan(
    reference_count: usize,
    reference_id_bytes: u64,
) -> Result<u64, ConversionError> {
    u64::try_from(reference_count)
        .unwrap_or(u64::MAX)
        .checked_mul(BYTES_PER_REFERENCE_ENTRY)
        .and_then(|bytes| bytes.checked_add(reference_id_bytes))
        .and_then(|bytes| bytes.checked_add(REFERENCE_BASE_BYTES))
        .ok_or_else(|| resource("max_memory_bytes", "embedded visual reference index overflow"))
}

#[derive(Clone, Copy)]
pub(super) struct OcrCandidate {
    pub(super) asset_index: usize,
    pub(super) reference_count: u64,
    pub(super) dimensions: (u32, u32),
    pub(super) digest: [u8; 32],
}

pub(super) struct CandidateBuffer {
    items: Vec<OcrCandidate>,
    capacity: usize,
    planned_bytes: u64,
    _memory: ResourceReservation,
}

impl CandidateBuffer {
    pub(super) fn new(
        capacity: usize,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let planned_bytes = candidate_buffer_plan(capacity)?;
        let memory = context.reserve_memory(planned_bytes)?;
        let mut items = Vec::new();
        items.try_reserve_exact(capacity).map_err(|error| {
            resource("max_memory_bytes", format!("allocate OCR candidate buffer: {error}"))
        })?;
        Ok(Self { items, capacity, planned_bytes, _memory: memory })
    }

    pub(super) fn push(&mut self, candidate: OcrCandidate) -> Result<(), ConversionError> {
        if self.items.len() == self.capacity {
            return Err(resource(
                "max_memory_bytes",
                "OCR candidate buffer exceeded its preflight plan",
            ));
        }
        self.items.push(candidate);
        Ok(())
    }

    pub(super) fn planned_bytes(&self) -> u64 {
        self.planned_bytes
    }
}

impl std::ops::Deref for CandidateBuffer {
    type Target = [OcrCandidate];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

pub(super) fn candidate_buffer_plan(candidate_capacity: usize) -> Result<u64, ConversionError> {
    u64::try_from(candidate_capacity)
        .unwrap_or(u64::MAX)
        .checked_mul(BYTES_PER_CANDIDATE_BUFFER_ENTRY)
        .and_then(|bytes| bytes.checked_add(CANDIDATE_BUFFER_BASE_BYTES))
        .ok_or_else(|| resource("max_memory_bytes", "OCR candidate buffer plan overflow"))
}

pub(super) struct CandidateGroup {
    pub(super) representative: usize,
    pub(super) reference_copies: u64,
    pub(super) asset_copies: u64,
}

pub(super) struct CandidateGrouping {
    pub(super) groups: Vec<CandidateGroup>,
    pub(super) membership: Vec<usize>,
    pub(super) digest_order: Vec<usize>,
    _memory: ResourceReservation,
}

pub(super) fn candidate_index_plan(candidate_count: usize) -> Result<u64, ConversionError> {
    u64::try_from(candidate_count)
        .unwrap_or(u64::MAX)
        .checked_mul(BYTES_PER_GROUPING_CANDIDATE)
        .and_then(|bytes| bytes.checked_add(GROUPING_BASE_BYTES))
        .ok_or_else(|| resource("max_memory_bytes", "OCR candidate index plan overflow"))
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct BytesKey<'a>(&'a [u8]);

impl Ord for BytesKey<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(test)]
        FULL_BYTE_COMPARISONS.set(FULL_BYTE_COMPARISONS.get() + 1);
        self.0.cmp(other.0)
    }
}

impl PartialOrd for BytesKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Group candidates by digest and then by exact payload. The digest map gives
/// logarithmic lookup for ordinary unique inputs. A borrowed full-byte index
/// keeps even a forced-collision bucket logarithmic without copying payloads;
/// complete byte ordering/equality remains the final identity authority.
pub(super) fn group_candidates(
    candidates: &[OcrCandidate],
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<CandidateGrouping, ConversionError> {
    let memory = context.reserve_memory(candidate_index_plan(candidates.len())?)?;
    let mut groups = Vec::<CandidateGroup>::new();
    groups.try_reserve_exact(candidates.len()).map_err(|error| {
        resource("max_memory_bytes", format!("allocate OCR candidate groups: {error}"))
    })?;
    let mut membership = Vec::<usize>::new();
    membership.try_reserve_exact(candidates.len()).map_err(|error| {
        resource("max_memory_bytes", format!("allocate OCR candidate membership: {error}"))
    })?;
    let mut buckets = BTreeMap::<[u8; 32], BTreeMap<BytesKey<'_>, usize>>::new();
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        context.checkpoint()?;
        let bucket = buckets.entry(candidate.digest).or_default();
        let bytes = BytesKey(&assets[candidate.asset_index].bytes);
        let group_index = match bucket.entry(bytes) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                let group_index = *entry.get();
                let group = &mut groups[group_index];
                group.reference_copies =
                    group.reference_copies.checked_add(candidate.reference_count).ok_or_else(
                        || resource("max_memory_bytes", "OCR reference-copy count overflow"),
                    )?;
                group.asset_copies = group
                    .asset_copies
                    .checked_add(1)
                    .ok_or_else(|| resource("max_memory_bytes", "OCR asset-copy count overflow"))?;
                group_index
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let group_index = groups.len();
                groups.push(CandidateGroup {
                    representative: candidate_index,
                    reference_copies: candidate.reference_count,
                    asset_copies: 1,
                });
                entry.insert(group_index);
                group_index
            }
        };
        membership.push(group_index);
    }
    let mut digest_order = Vec::new();
    digest_order.try_reserve_exact(groups.len()).map_err(|error| {
        resource("max_memory_bytes", format!("allocate OCR candidate order: {error}"))
    })?;
    for bucket in buckets.values() {
        digest_order.extend(bucket.values().copied());
    }
    Ok(CandidateGrouping { groups, membership, digest_order, _memory: memory })
}

#[cfg(test)]
mod tests {
    use super::super::{image_dimensions, plan_enrichment};
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use into_markdown_core::{
        Block, BlockNode, BoxFuture, ConversionOptions, ConverterOutput, Document, EnrichmentPlan,
        ExecutionOptions, InputFormat, NodeId, OcrEngine, OcrOutputPlan, OcrPolicy, OcrRecognition,
        OcrRequest, OcrResult, Provenance, ProvenanceKind, ResourceLimits, Services, SourceLocator,
    };
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    struct PlanningOrderOcr {
        dimensions: Mutex<Vec<(u32, u32)>>,
        fail_at: Option<(u32, u32)>,
    }

    impl OcrEngine for PlanningOrderOcr {
        fn id(&self) -> &'static str {
            "test.ocr.planning-order"
        }

        fn recognize<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            Box::pin(async { unreachable!("planning-order provider never recognizes") })
        }

        fn planned_bound_output(
            &self,
            _: OcrRequest<'_>,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            unreachable!("embedded OCR uses the normalized-PNG plan")
        }

        fn planned_normalized_png_output(
            &self,
            width: u32,
            height: u32,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            self.dimensions.lock().unwrap().push((width, height));
            if self.fail_at == Some((width, height)) {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "planning-order sentinel".into(),
                });
            }
            OcrOutputPlan::try_new(1024, 1, 128)
        }

        fn recognize_bound<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
            Box::pin(async { unreachable!("planning-order provider never recognizes") })
        }
    }

    fn asset(index: usize, bytes: Vec<u8>) -> Asset {
        Asset {
            id: AssetId(format!("asset-{index}")),
            filename: None,
            media_type: "image/png".into(),
            bytes,
            external_uri: None,
        }
    }

    fn reference_plan(ids: &[AssetId]) -> (usize, u64) {
        (ids.len(), ids.iter().map(|id| u64::try_from(id.0.len()).unwrap()).sum())
    }

    fn build_references(
        ids: &[AssetId],
        context: &ExecutionContext,
    ) -> Result<ReferenceIndex, ConversionError> {
        let (count, bytes) = reference_plan(ids);
        let mut references = ReferenceIndex::new(count, bytes, context)?;
        for id in ids {
            references.record(id)?;
        }
        Ok(references)
    }

    fn png_with_dimensions(width: u32, height: u32, value: u8) -> Vec<u8> {
        let pixels = RgbaImage::from_pixel(width, height, Rgba([value, 2, 3, 255]));
        let mut cursor = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels).write_to(&mut cursor, ImageFormat::Png).unwrap();
        cursor.into_inner()
    }

    fn planning_output(first: Vec<u8>, second: Vec<u8>) -> ConverterOutput {
        let provenance = || Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "test.converter".into(),
            locator: SourceLocator::default(),
            confidence: Some(1.0),
        };
        ConverterOutput::new(
            Document {
                blocks: vec![
                    BlockNode {
                        id: NodeId("image-first".into()),
                        block: Block::Image { asset: AssetId("asset-first".into()), alt: None },
                        provenance: provenance(),
                    },
                    BlockNode {
                        id: NodeId("image-second".into()),
                        block: Block::Image { asset: AssetId("asset-second".into()), alt: None },
                        provenance: provenance(),
                    },
                ],
                ..Document::default()
            },
            vec![
                Asset {
                    id: AssetId("asset-first".into()),
                    filename: Some("first.png".into()),
                    media_type: "image/png".into(),
                    bytes: first,
                    external_uri: None,
                },
                Asset {
                    id: AssetId("asset-second".into()),
                    filename: Some("second.png".into()),
                    media_type: "image/png".into(),
                    bytes: second,
                    external_uri: None,
                },
            ],
            vec![],
        )
    }

    #[test]
    fn provider_preflight_preserves_asset_order_and_first_error() {
        let mut first_bytes = png_with_dimensions(2, 3, 11);
        let mut second_bytes = png_with_dimensions(7, 5, 29);
        if Sha256::digest(&first_bytes) < Sha256::digest(&second_bytes) {
            std::mem::swap(&mut first_bytes, &mut second_bytes);
        }
        let first_dimensions = image_dimensions(&first_bytes).unwrap();
        let second_dimensions = image_dimensions(&second_bytes).unwrap();
        assert!(Sha256::digest(&first_bytes) > Sha256::digest(&second_bytes));
        let source = planning_output(first_bytes, second_bytes);
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let recorder =
            Arc::new(PlanningOrderOcr { dimensions: Mutex::new(Vec::new()), fail_at: None });
        let services = Services { ocr: Some(recorder.clone()), ..Services::default() };
        assert!(matches!(
            plan_enrichment(&source, InputFormat::Docx, &options, &services, &context).unwrap(),
            EnrichmentPlan::Reserve(_)
        ));
        assert_eq!(*recorder.dimensions.lock().unwrap(), [first_dimensions, second_dimensions]);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let failing = Arc::new(PlanningOrderOcr {
            dimensions: Mutex::new(Vec::new()),
            fail_at: Some(first_dimensions),
        });
        let services = Services { ocr: Some(failing.clone()), ..Services::default() };
        let error =
            plan_enrichment(&source, InputFormat::Docx, &options, &services, &context).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ResourceLimit { limit: "max_memory_bytes", detail }
                if detail == "planning-order sentinel"
        ));
        assert_eq!(*failing.dimensions.lock().unwrap(), [first_dimensions]);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn low_memory_preserves_first_dangling_reference_before_index_reservation() {
        let first_bytes = png_with_dimensions(2, 3, 11);
        let second_bytes = png_with_dimensions(7, 5, 29);
        let complete = planning_output(first_bytes.clone(), second_bytes.clone());
        let mut dangling = planning_output(first_bytes, second_bytes);
        let Block::Image { asset, .. } = &mut dangling.document.blocks[0].block else {
            unreachable!("fixture starts with an image")
        };
        *asset = AssetId("z-missing".into());
        let Block::Image { asset, .. } = &mut dangling.document.blocks[1].block else {
            unreachable!("fixture ends with an image")
        };
        *asset = AssetId("a-missing".into());

        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.limits.max_memory_bytes = 1;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error =
            plan_enrichment(&dangling, InputFormat::Docx, &options, &Services::default(), &context)
                .unwrap_err();
        assert!(matches!(
            error,
            ConversionError::Internal { detail }
                if detail == "image node references missing asset z-missing"
        ));
        assert_eq!(context.reserved_memory_bytes(), 0);

        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            plan_enrichment(&complete, InputFormat::Docx, &options, &Services::default(), &context,),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn referenced_asset_validation_scans_8192_assets_once() {
        const ASSETS: usize = 8192;
        let assets = (0..ASSETS)
            .map(|index| Asset {
                id: AssetId(format!("asset-{index}-{}", "long-id".repeat(32))),
                ..asset(index, Vec::new())
            })
            .collect::<Vec<_>>();
        let ids = assets.iter().map(|asset| asset.id.clone()).collect::<Vec<_>>();
        let (count, id_bytes) = reference_plan(&ids);
        let exact = reference_index_plan(count, id_bytes).unwrap();
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact, ..ResourceLimits::default() },
        );
        let mut references = build_references(&ids, &context).unwrap();

        ASSET_LOOKUPS.set(0);
        references.validate_assets(&assets, &context).unwrap();
        assert_eq!(ASSET_LOOKUPS.get(), ASSETS);
        assert_eq!(context.reserved_memory_bytes(), exact);
        drop(references);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let low = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact - 1, ..ResourceLimits::default() },
        );
        assert!(matches!(
            build_references(&ids, &low),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(low.reserved_memory_bytes(), 0);
    }

    #[test]
    fn reference_validation_preserves_missing_and_duplicate_id_semantics() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let duplicate = asset(0, Vec::new());
        let mut references =
            build_references(std::slice::from_ref(&duplicate.id), &context).unwrap();
        references.validate_assets(&[duplicate.clone(), duplicate], &context).unwrap();
        drop(references);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let missing = [AssetId("z-missing".into()), AssetId("a-missing".into())];
        let mut references = build_references(&missing, &context).unwrap();
        let error = references.validate_assets(&[], &context).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::Internal { detail }
                if detail == "image node references missing asset z-missing"
        ));
        drop(references);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn reference_and_candidate_leases_cover_exact_simultaneous_peak_and_errors() {
        let ids = [AssetId("first-long-reference".repeat(64))];
        let (count, id_bytes) = reference_plan(&ids);
        let reference_bytes = reference_index_plan(count, id_bytes).unwrap();
        let candidate_bytes = candidate_buffer_plan(count).unwrap();
        let grouping_bytes = candidate_index_plan(count).unwrap();
        let exact = reference_bytes + candidate_bytes + grouping_bytes;
        let assets = vec![asset(0, b"payload".to_vec())];
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact, ..ResourceLimits::default() },
        );
        let references = build_references(&ids, &context).unwrap();
        let mut candidates = CandidateBuffer::new(count, &context).unwrap();
        candidates
            .push(OcrCandidate {
                asset_index: 0,
                reference_count: 1,
                dimensions: (1, 1),
                digest: Sha256::digest(&assets[0].bytes).into(),
            })
            .unwrap();
        let grouping = group_candidates(&candidates, &assets, &context).unwrap();
        assert_eq!(context.reserved_memory_bytes(), exact);
        drop(grouping);
        drop(candidates);
        drop(references);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let low = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact - 1, ..ResourceLimits::default() },
        );
        let references = build_references(&ids, &low).unwrap();
        let mut candidates = CandidateBuffer::new(count, &low).unwrap();
        candidates
            .push(OcrCandidate {
                asset_index: 0,
                reference_count: 1,
                dimensions: (1, 1),
                digest: Sha256::digest(&assets[0].bytes).into(),
            })
            .unwrap();
        assert!(matches!(
            group_candidates(&candidates, &assets, &low),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        drop(candidates);
        drop(references);
        assert_eq!(low.reserved_memory_bytes(), 0);

        let cancellation = into_markdown_core::CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert!(matches!(build_references(&ids, &cancelled), Err(ConversionError::Cancelled)));
        assert!(matches!(CandidateBuffer::new(count, &cancelled), Err(ConversionError::Cancelled)));
        assert_eq!(cancelled.reserved_memory_bytes(), 0);

        let bounded = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: reference_bytes, ..ResourceLimits::default() },
        );
        let mut references = ReferenceIndex::new(count, id_bytes, &bounded).unwrap();
        references.record(&ids[0]).unwrap();
        assert!(matches!(
            references.record(&ids[0]),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        drop(references);
        assert_eq!(bounded.reserved_memory_bytes(), 0);
    }

    #[test]
    fn index_is_linear_for_4096_unique_assets_and_collision_safe() {
        const ASSETS: usize = 4096;
        let assets = (0..ASSETS)
            .map(|index| asset(index, u64::try_from(index).unwrap().to_le_bytes().to_vec()))
            .collect::<Vec<_>>();
        let candidates = assets
            .iter()
            .enumerate()
            .map(|(asset_index, asset)| OcrCandidate {
                asset_index,
                reference_count: 1,
                dimensions: (1, 1),
                digest: Sha256::digest(&asset.bytes).into(),
            })
            .collect::<Vec<_>>();
        let exact = candidate_index_plan(candidates.len()).unwrap();
        let exact_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact, ..ResourceLimits::default() },
        );
        FULL_BYTE_COMPARISONS.set(0);
        let grouping = group_candidates(&candidates, &assets, &exact_context).unwrap();
        assert_eq!(grouping.groups.len(), ASSETS);
        assert_eq!(grouping.membership.len(), ASSETS);
        assert_eq!(grouping.digest_order.len(), ASSETS);
        assert_eq!(FULL_BYTE_COMPARISONS.get(), 0);
        assert_eq!(exact_context.reserved_memory_bytes(), exact);
        drop(grouping);
        assert_eq!(exact_context.reserved_memory_bytes(), 0);

        let low_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: exact - 1, ..ResourceLimits::default() },
        );
        assert!(matches!(
            group_candidates(&candidates, &assets, &low_context),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(low_context.reserved_memory_bytes(), 0);

        let collision_assets = vec![
            asset(0, b"same".to_vec()),
            asset(1, b"different".to_vec()),
            asset(2, b"same".to_vec()),
        ];
        let collision_candidates = (0..3)
            .map(|asset_index| OcrCandidate {
                asset_index,
                reference_count: u64::try_from(asset_index + 1).unwrap(),
                dimensions: (1, 1),
                digest: [7; 32],
            })
            .collect::<Vec<_>>();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        FULL_BYTE_COMPARISONS.set(0);
        let collision =
            group_candidates(&collision_candidates, &collision_assets, &context).unwrap();
        assert_eq!(collision.groups.len(), 2);
        assert_eq!(collision.membership, [0, 1, 0]);
        assert_eq!(collision.groups[0].reference_copies, 4);
        assert_eq!(collision.groups[0].asset_copies, 2);
        assert_eq!(collision.groups[1].reference_copies, 2);
        assert!(FULL_BYTE_COMPARISONS.get() > 0);
        assert!(FULL_BYTE_COMPARISONS.get() < 16);

        let forced_collision_candidates = candidates
            .iter()
            .map(|candidate| OcrCandidate { digest: [9; 32], ..*candidate })
            .collect::<Vec<_>>();
        FULL_BYTE_COMPARISONS.set(0);
        let forced = group_candidates(&forced_collision_candidates, &assets, &context).unwrap();
        assert_eq!(forced.groups.len(), ASSETS);
        assert!(
            FULL_BYTE_COMPARISONS.get() < ASSETS * 64,
            "forced digest collisions remain logarithmic instead of quadratic"
        );
    }

    #[test]
    fn digest_order_and_membership_match_legacy_non_collision_semantics() {
        let assets =
            vec![asset(0, b"z".to_vec()), asset(1, b"a".to_vec()), asset(2, b"z".to_vec())];
        let candidates = assets
            .iter()
            .enumerate()
            .map(|(asset_index, asset)| OcrCandidate {
                asset_index,
                reference_count: 1,
                dimensions: (1, 1),
                digest: Sha256::digest(&asset.bytes).into(),
            })
            .collect::<Vec<_>>();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let grouping = group_candidates(&candidates, &assets, &context).unwrap();
        let ordered_digests = grouping
            .digest_order
            .iter()
            .map(|group| candidates[grouping.groups[*group].representative].digest)
            .collect::<Vec<_>>();
        let mut expected = candidates.iter().map(|candidate| candidate.digest).collect::<Vec<_>>();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(ordered_digests, expected);
        assert_eq!(grouping.membership[0], grouping.membership[2]);
        assert_ne!(grouping.membership[0], grouping.membership[1]);
    }

    #[test]
    fn cancellation_releases_the_candidate_index_lease() {
        let assets = vec![asset(0, b"one".to_vec())];
        let candidates = vec![OcrCandidate {
            asset_index: 0,
            reference_count: 1,
            dimensions: (1, 1),
            digest: Sha256::digest(&assets[0].bytes).into(),
        }];
        let cancellation = into_markdown_core::CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );

        assert!(matches!(
            group_candidates(&candidates, &assets, &context),
            Err(ConversionError::Cancelled)
        ));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}
