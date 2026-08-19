//! Offline, bounded Whisper-small speech recognition.
//!
//! Encoded media is normalized by the audited FFmpeg runtime. Model bytes are
//! resolved only through `ModelManager`; ordinary transcription never downloads.

use into_markdown_core::{
    AsrOptions, Block, BlockNode, BoxFuture, ConversionError, ExecutionContext, ExecutionStage,
    Inline, NodeId, Provenance, ProvenanceKind, ResourceReservation, SourceLocator, TimeRange,
    Transcriber, TranscriptionRequest, TranscriptionResult,
};
use into_markdown_ffmpeg::{FfmpegRuntime, MediaLimits, PcmAudio};
use into_markdown_ocr::{ModelManager, ModelManagerError};
use std::sync::{Arc, Mutex, MutexGuard};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const PROVIDER_ID: &str = "builtin.asr.whisper-small";
const SAMPLE_RATE: u32 = 16_000;
const MAX_THREADS: u16 = 8;
const MAX_DURATION_MS: u64 = 30 * 60 * 1_000;
const MAX_SEGMENTS: u32 = 100_000;
const MIN_NATIVE_MEMORY: u64 = 256 * 1024 * 1024;
const MAX_NATIVE_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024 * 1024;

/// CPU-only limits and model selection for one installed service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperConfig {
    /// Embedded model bundle ID.
    pub model_bundle: String,
    /// Optional normalized Whisper language code.
    pub language: Option<String>,
    /// Maximum decoder threads.
    pub max_threads: u16,
    /// Maximum decoded duration.
    pub max_duration_ms: u64,
    /// Maximum retained timed segments.
    pub max_segments: u32,
    /// Conservative native model/decoder memory reservation.
    pub max_native_memory_bytes: u64,
}

impl TryFrom<&AsrOptions> for WhisperConfig {
    type Error = ConversionError;

    fn try_from(options: &AsrOptions) -> Result<Self, Self::Error> {
        let language = options.language.as_deref().map(normalize_language).transpose()?;
        let config = Self {
            model_bundle: options.model_bundle.clone(),
            language,
            max_threads: options.max_threads,
            max_duration_ms: options.max_duration_ms,
            max_segments: options.max_segments,
            max_native_memory_bytes: options.max_native_memory_bytes,
        };
        config.validate()?;
        Ok(config)
    }
}

impl WhisperConfig {
    fn validate(&self) -> Result<(), ConversionError> {
        if self.model_bundle.is_empty()
            || !(1..=MAX_THREADS).contains(&self.max_threads)
            || !(1..=MAX_DURATION_MS).contains(&self.max_duration_ms)
            || !(1..=MAX_SEGMENTS).contains(&self.max_segments)
            || !(MIN_NATIVE_MEMORY..=MAX_NATIVE_MEMORY).contains(&self.max_native_memory_bytes)
        {
            return Err(resource("asrConfiguration"));
        }
        Ok(())
    }
}

struct CachedModel {
    context: WhisperContext,
    identity: String,
}

/// Installed, offline Whisper-small provider with a single-flight model cache.
pub struct WhisperSmallTranscriber {
    manager: Arc<ModelManager>,
    ffmpeg: Arc<FfmpegRuntime>,
    config: WhisperConfig,
    cache: Mutex<Option<CachedModel>>,
}

impl WhisperSmallTranscriber {
    /// Construct without loading native model state or accessing the network.
    pub fn new(
        manager: Arc<ModelManager>,
        ffmpeg: Arc<FfmpegRuntime>,
        config: WhisperConfig,
    ) -> Result<Self, ConversionError> {
        config.validate()?;
        let default = manager.manifest().default_asr_bundle.as_deref().ok_or_else(|| {
            component(&config.model_bundle, "embedded ASR model authority is absent")
        })?;
        if config.model_bundle != default {
            return Err(component(
                &config.model_bundle,
                "selected bundle is not the reviewed Whisper-small model",
            ));
        }
        Ok(Self { manager, ffmpeg, config, cache: Mutex::new(None) })
    }

    fn model<'a>(
        &'a self,
        context: &ExecutionContext,
    ) -> Result<MutexGuard<'a, Option<CachedModel>>, ConversionError> {
        let mut cache = lock(&self.cache);
        if cache.is_some() {
            return Ok(cache);
        }
        context.checkpoint()?;
        let artifact = self
            .manager
            .verified_runtime_path(&self.config.model_bundle, "model", context)
            .map_err(|error| model_error(&self.config.model_bundle, error))?;
        let mut parameters = WhisperContextParameters::default();
        parameters.use_gpu(false).flash_attn(false);
        let native = WhisperContext::new_with_params(&artifact.path, parameters).map_err(|_| {
            component(&self.config.model_bundle, "verified Whisper model could not be loaded")
        })?;
        if !native.is_multilingual()
            || native.model_type_readable_str().ok().is_none_or(|value| value != "small")
        {
            return Err(component(
                &self.config.model_bundle,
                "verified model is not multilingual Whisper small",
            ));
        }
        // The model data directory is protected, but a concurrent manager may
        // publish/remove by rename. Reverification makes that race fail closed.
        self.manager
            .verify_with_context(&self.config.model_bundle, context)
            .map_err(|error| model_error(&self.config.model_bundle, error))?;
        *cache = Some(CachedModel {
            context: native,
            identity: format!("{}@sha256:{}", self.config.model_bundle, artifact.sha256),
        });
        Ok(cache)
    }

    fn transcribe_sync(
        &self,
        request: TranscriptionRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<TranscriptionResult, ConversionError> {
        context.checkpoint()?;
        self.config.validate()?;
        // The cache is process-wide, but every use must independently prove
        // that this request's memory budget can accommodate the native model.
        let _native_memory = context.reserve_memory(self.config.max_native_memory_bytes)?;
        context.report(ExecutionStage::Ai, Some(0), Some(100), Some("asr.normalize"))?;
        let pcm = self.ffmpeg.normalize(
            request.media,
            MediaLimits {
                max_input_bytes: u64::try_from(request.media.len())
                    .map_err(|_| resource("asrInputBytes"))?,
                max_duration_ms: self.config.max_duration_ms,
                sample_rate: SAMPLE_RATE,
                channels: 1,
                ..MediaLimits::default()
            },
            context,
        )?;
        if pcm.frames.saturating_mul(1_000) / u64::from(SAMPLE_RATE) > self.config.max_duration_ms {
            return Err(resource("asrDuration"));
        }
        let samples = pcm_f32(&pcm, context)?;
        let mut cache = self.model(context)?;
        let model = cache.as_mut().ok_or_else(|| component(PROVIDER_ID, "model cache failed"))?;
        let mut state = model
            .context
            .create_state()
            .map_err(|_| component(PROVIDER_ID, "Whisper decoder state could not be created"))?;
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(usize::from(self.config.max_threads));
        let requested_language = request
            .language
            .map(normalize_language)
            .transpose()?
            .or_else(|| self.config.language.clone());
        let (language, language_confidence) = if let Some(language) = requested_language {
            (Some(language), None)
        } else {
            state
                .pcm_to_mel(&samples.values, threads)
                .map_err(|_| component(PROVIDER_ID, "language detection preprocessing failed"))?;
            let (id, probabilities) = state
                .lang_detect(0, threads)
                .map_err(|_| component(PROVIDER_ID, "language detection failed"))?;
            let language = whisper_rs::get_lang_str(id).map(str::to_owned).ok_or_else(|| {
                component(PROVIDER_ID, "language detection returned an unknown language")
            })?;
            let confidence = probabilities
                .get(usize::try_from(id).map_err(|_| component(PROVIDER_ID, "invalid language"))?)
                .copied()
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 1.0));
            (Some(language), confidence)
        };
        let mut params =
            FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
        params.set_n_threads(i32::try_from(threads).map_err(|_| resource("asrConfiguration"))?);
        params.set_translate(false);
        params.set_no_timestamps(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_language(language.as_deref());
        install_callbacks(&mut params, context);
        if state.full(params, &samples.values).is_err() {
            context.checkpoint()?;
            return Err(component(PROVIDER_ID, "Whisper inference failed"));
        }
        context.checkpoint()?;
        let segments = collect_segments(
            &state,
            self.config.max_segments,
            self.config.max_duration_ms,
            &model.identity,
            context,
        )?;
        context.report(ExecutionStage::Ai, Some(100), Some(100), Some("asr.complete"))?;
        Ok(TranscriptionResult {
            segments,
            provider: PROVIDER_ID.into(),
            model: model.identity.clone(),
            language,
            language_confidence,
        })
    }
}

fn install_callbacks(params: &mut FullParams<'_, '_>, context: &ExecutionContext) {
    let abort_context = context.clone();
    params.set_abort_callback_safe(move || abort_context.checkpoint().is_err());
    let progress_context = context.clone();
    params.set_progress_callback_safe(move |progress: i32| {
        let progress = u64::try_from(progress.clamp(0, 100)).unwrap_or_default();
        let _ = progress_context.report(
            ExecutionStage::Ai,
            Some(progress),
            Some(100),
            Some("asr.inference"),
        );
    });
}

impl Transcriber for WhisperSmallTranscriber {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        Box::pin(async move { self.transcribe_sync(request, context) })
    }
}

struct AccountedSamples {
    values: Vec<f32>,
    _memory: ResourceReservation,
}

fn pcm_f32(
    pcm: &PcmAudio,
    context: &ExecutionContext,
) -> Result<AccountedSamples, ConversionError> {
    if pcm.sample_rate != SAMPLE_RATE || pcm.channels != 1 || pcm.samples().len() % 2 != 0 {
        return Err(component(PROVIDER_ID, "FFmpeg returned an invalid PCM contract"));
    }
    let sample_count = pcm.samples().len() / 2;
    let bytes = u64::try_from(sample_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| resource("asrPcmSamples"))?;
    let memory = context.reserve_memory(bytes)?;
    let mut samples = Vec::new();
    samples.try_reserve_exact(sample_count).map_err(|_| resource("asrPcmSamples"))?;
    for chunk in pcm.samples().chunks_exact(2) {
        context.checkpoint()?;
        samples.push(f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32768.0);
    }
    Ok(AccountedSamples { values: samples, _memory: memory })
}

fn collect_segments(
    state: &whisper_rs::WhisperState,
    maximum: u32,
    duration_ms: u64,
    model: &str,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let count = u32::try_from(state.full_n_segments()).map_err(|_| resource("asrSegments"))?;
    if count > maximum {
        return Err(resource("asrSegments"));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(usize::try_from(count).map_err(|_| resource("asrSegments"))?)
        .map_err(|_| resource("asrSegments"))?;
    let mut transcript_bytes = 0_usize;
    let mut previous_end = 0_u64;
    for (index, segment) in state.as_iter().enumerate() {
        context.checkpoint()?;
        let text = segment
            .to_str_lossy()
            .map_err(|_| component(PROVIDER_ID, "Whisper returned invalid segment text"))?
            .trim()
            .to_owned();
        if text.is_empty() {
            continue;
        }
        transcript_bytes = transcript_bytes
            .checked_add(text.len())
            .filter(|total| *total <= MAX_TRANSCRIPT_BYTES)
            .ok_or_else(|| resource("asrTranscriptBytes"))?;
        let start_ms = u64::try_from(segment.start_timestamp())
            .ok()
            .and_then(|value| value.checked_mul(10))
            .ok_or_else(|| component(PROVIDER_ID, "invalid segment timestamp"))?;
        let end_ms = u64::try_from(segment.end_timestamp())
            .ok()
            .and_then(|value| value.checked_mul(10))
            .ok_or_else(|| component(PROVIDER_ID, "invalid segment timestamp"))?;
        if start_ms < previous_end || end_ms <= start_ms || end_ms > duration_ms {
            return Err(component(PROVIDER_ID, "non-monotonic segment timestamp"));
        }
        previous_end = end_ms;
        let confidence = segment_confidence(&segment);
        let range = TimeRange { start_ms, end_ms };
        output.push(BlockNode {
            id: NodeId(format!("asr-segment-{:06}", index + 1)),
            block: Block::TimedSegment {
                range,
                speaker: None,
                content: vec![Inline::Text { value: text, marks: Vec::new() }],
            },
            provenance: Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: format!("{PROVIDER_ID}/{model}"),
                locator: SourceLocator { time: Some(range), ..SourceLocator::default() },
                confidence: Some(confidence),
            },
        });
    }
    Ok(output)
}

fn segment_confidence(segment: &whisper_rs::WhisperSegment<'_>) -> f32 {
    let mut total = 0.0_f64;
    let mut count = 0_u32;
    for index in 0..segment.n_tokens() {
        if let Some(token) = segment.get_token(index) {
            let probability = token.token_probability();
            if probability.is_finite() {
                total += f64::from(probability.clamp(0.0, 1.0));
                count += 1;
            }
        }
    }
    if count == 0 { 0.0 } else { (total / f64::from(count)) as f32 }
}

fn normalize_language(value: &str) -> Result<String, ConversionError> {
    if value.is_empty()
        || value.len() > 35
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(component(PROVIDER_ID, "invalid ASR language hint"));
    }
    let primary = value.split('-').next().unwrap_or_default().to_ascii_lowercase();
    let normalized = match primary.as_str() {
        "cmn" | "yue" => "zh",
        other => other,
    };
    if whisper_rs::get_lang_id(normalized).is_none() {
        return Err(component(PROVIDER_ID, "unsupported ASR language hint"));
    }
    Ok(normalized.to_owned())
}

fn model_error(bundle: &str, error: ModelManagerError) -> ConversionError {
    match error {
        ModelManagerError::Execution(error) => error,
        ModelManagerError::NotInstalled | ModelManagerError::UnknownBundle => component(
            bundle,
            &format!("Whisper model is not installed; run `into-md models install {bundle}`"),
        ),
        ModelManagerError::Corrupt(_) => component(
            bundle,
            &format!("Whisper model is corrupt; reinstall with `into-md models install {bundle}`"),
        ),
        error => component(bundle, &format!("Whisper model verification failed: {error}")),
    }
}

fn component(component_name: &str, detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: component_name.into(),
        detail: detail.into(),
    }
}

fn resource(limit: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: "ASR resource policy exceeded".into() }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ErrorCode, ResourceLimits};

    #[test]
    fn language_hints_are_bounded_and_normalized() {
        assert_eq!(normalize_language("zh-Hans").unwrap(), "zh");
        assert_eq!(normalize_language("en-US").unwrap(), "en");
        assert_eq!(normalize_language("cmn").unwrap(), "zh");
        assert_eq!(
            normalize_language("en\0zh").unwrap_err().code(),
            ErrorCode::ComponentUnavailable
        );
        assert_eq!(
            normalize_language("not-a-language").unwrap_err().code(),
            ErrorCode::ComponentUnavailable
        );
    }

    #[test]
    fn policy_limits_fail_before_model_or_media_work() {
        let mut options = AsrOptions::default();
        options.max_threads = 0;
        assert_eq!(WhisperConfig::try_from(&options).unwrap_err().code(), ErrorCode::ResourceLimit);
        options.max_threads = 1;
        options.max_duration_ms = MAX_DURATION_MS + 1;
        assert!(WhisperConfig::try_from(&options).is_err());
        options.max_duration_ms = 1;
        options.max_native_memory_bytes = MIN_NATIVE_MEMORY - 1;
        assert!(WhisperConfig::try_from(&options).is_err());
    }

    #[test]
    fn missing_model_maps_to_stable_component_error_with_install_hint() {
        let error = model_error("whisper-small-multilingual", ModelManagerError::NotInstalled);
        assert_eq!(error.code(), ErrorCode::ComponentUnavailable);
        assert!(error.to_string().contains("models install whisper-small-multilingual"));
        let corrupt =
            model_error("whisper-small-multilingual", ModelManagerError::Corrupt("hash".into()));
        assert_eq!(corrupt.code(), ErrorCode::ComponentUnavailable);
        assert!(corrupt.to_string().contains("corrupt"));
    }

    #[test]
    fn default_memory_policy_is_explicitly_reservable_or_fails_closed() {
        let options = AsrOptions::default();
        let context = ExecutionContext::new(Default::default(), ResourceLimits::default());
        let reservation = context.reserve_memory(options.max_native_memory_bytes).unwrap();
        assert_eq!(context.reserved_memory_bytes(), options.max_native_memory_bytes);
        drop(reservation);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn request_context_callbacks_match_whisper_safe_api() {
        let context = ExecutionContext::new(Default::default(), ResourceLimits::default());
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        install_callbacks(&mut params, &context);
    }

    #[test]
    fn request_callbacks_release_captured_resources_when_params_drop() {
        for _ in 0..256 {
            let abort_resource = Arc::new(());
            let abort_weak = Arc::downgrade(&abort_resource);
            let progress_resource = Arc::new(());
            let progress_weak = Arc::downgrade(&progress_resource);
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_abort_callback_safe(move || {
                let _resource = &abort_resource;
                false
            });
            params.set_progress_callback_safe(move |_| {
                let _resource = &progress_resource;
            });
            drop(params);
            assert!(abort_weak.upgrade().is_none());
            assert!(progress_weak.upgrade().is_none());
        }
    }
}
