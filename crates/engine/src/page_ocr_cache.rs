//! Request-local OCR contributions shared by transient PDF pages, never pixels.

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ExecutionContext, OcrEngine, OcrInputIdentity,
    OcrOutputPlan, OcrRecognition, OcrRegion, OcrRequest, OcrResult, ProvenanceKind,
    ResourceReservation,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

struct Entry {
    contribution: OcrRecognition,
    normalized_sha256: [u8; 32],
    counted: bool,
    _memory: ResourceReservation,
}

pub(crate) struct PageOcrCache {
    provider: Arc<dyn OcrEngine>,
    entries: Mutex<BTreeMap<[u8; 32], Entry>>,
}

impl PageOcrCache {
    pub(crate) fn new(provider: Arc<dyn OcrEngine>) -> Self {
        Self { provider, entries: Mutex::new(BTreeMap::new()) }
    }
}

impl OcrEngine for PageOcrCache {
    fn id(&self) -> &str {
        self.provider.id()
    }

    fn provenance_kind(&self) -> ProvenanceKind {
        self.provider.provenance_kind()
    }

    fn record_contribution(
        &self,
        input: OcrInputIdentity,
        regions: u64,
        characters: u64,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let mut entries =
            self.entries.lock().map_err(|_| cache_error("OCR contribution cache lock poisoned"))?;
        let Some(entry) =
            entries.values_mut().find(|entry| entry.normalized_sha256 == input.sha256())
        else {
            return context.record_ocr_contribution(regions, characters);
        };
        if !entry.counted {
            self.provider.record_contribution(input, regions, characters, context)?;
            entry.counted = true;
        }
        Ok(())
    }

    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        self.provider.recognize(request, context)
    }

    fn planned_bound_output(
        &self,
        request: OcrRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        cache_plan(self.provider.planned_bound_output(request, options, context)?)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        cache_plan(self.provider.planned_normalized_png_output(width, height, options, context)?)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            let digest = request_digest(request, context)?;
            {
                let entries = self
                    .entries
                    .lock()
                    .map_err(|_| cache_error("OCR contribution cache lock poisoned"))?;
                if let Some(entry) = entries.get(&digest) {
                    // The caller's normal provider-output allowance covers this
                    // clone; the cache keeps its own independently held lease.
                    return Ok(entry.contribution.clone());
                }
                if entries.len()
                    >= usize::try_from(context.resource_limits().max_pages).unwrap_or(usize::MAX)
                {
                    return Err(ConversionError::ResourceLimit {
                        limit: "max_pages",
                        detail: "request-wide PDF OCR candidate count exceeded".into(),
                    });
                }
            }
            let contribution = self.provider.recognize_bound(request, context).await?;
            context.checkpoint()?;
            let bytes = clone_bytes(&contribution)?
                .checked_add(entry_overhead())
                .ok_or_else(|| cache_error("OCR contribution cache size overflow"))?;
            let memory = context.reserve_memory(bytes)?;
            let cached = Entry {
                contribution: contribution.clone(),
                normalized_sha256: Sha256::digest(request.image).into(),
                counted: false,
                _memory: memory,
            };
            self.entries
                .lock()
                .map_err(|_| cache_error("OCR contribution cache lock poisoned"))?
                .insert(digest, cached);
            Ok(contribution)
        })
    }
}

fn cache_plan(plan: OcrOutputPlan) -> Result<OcrOutputPlan, ConversionError> {
    let working = plan
        .max_working_bytes()
        .checked_add(plan.max_retained_bytes())
        .and_then(|bytes| bytes.checked_add(entry_overhead()))
        .ok_or_else(|| cache_error("OCR contribution cache plan overflow"))?;
    OcrOutputPlan::try_new_with_working(
        plan.max_retained_bytes(),
        working,
        plan.max_regions(),
        plan.max_text_bytes(),
    )
}

fn entry_overhead() -> u64 {
    // One complete high-water B-tree node per entry, including sparse nodes.
    ((size_of::<[u8; 32]>() + size_of::<Entry>()) * 11 + size_of::<usize>() * 12 + 256) as u64
}

fn request_digest(
    request: OcrRequest<'_>,
    context: &ExecutionContext,
) -> Result<[u8; 32], ConversionError> {
    let mut hash = Sha256::new();
    hash.update((request.media_type.len() as u64).to_le_bytes());
    hash.update(request.media_type.as_bytes());
    hash.update((request.languages.len() as u64).to_le_bytes());
    for language in request.languages {
        hash.update((language.len() as u64).to_le_bytes());
        hash.update(language.as_bytes());
    }
    for chunk in request.image.chunks(64 * 1024) {
        context.checkpoint()?;
        hash.update(chunk);
    }
    Ok(hash.finalize().into())
}

fn clone_bytes(value: &OcrRecognition) -> Result<u64, ConversionError> {
    let mut bytes = size_of::<OcrRecognition>() as u64;
    let result = match value {
        OcrRecognition::Bound(bound) => {
            add(&mut bytes, size_of_val(bound.detection_confidences()))?;
            add(&mut bytes, size_of_val(bound.evidence_chain()))?;
            for step in bound.evidence_chain() {
                add(&mut bytes, step.provider.len())?;
                add(&mut bytes, step.model.as_ref().map_or(0, String::len))?;
            }
            bound.result()
        }
        OcrRecognition::Unbound(result) | OcrRecognition::Remote(result) => result,
        _ => return Err(cache_error("unsupported OCR contribution kind")),
    };
    add(&mut bytes, result.provider.len())?;
    add(&mut bytes, result.regions.len() * size_of::<OcrRegion>())?;
    for region in &result.regions {
        add(&mut bytes, region.text.len())?;
    }
    Ok(bytes)
}

fn add(bytes: &mut u64, extra: usize) -> Result<(), ConversionError> {
    *bytes = bytes
        .checked_add(u64::try_from(extra).map_err(|_| cache_error("OCR cache length overflow"))?)
        .ok_or_else(|| cache_error("OCR cache size overflow"))?;
    Ok(())
}

fn cache_error(detail: &str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
