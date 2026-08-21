//! Offline, bounded Whisper-small speech recognition.
//!
//! Encoded media is normalized by the audited `FFmpeg` runtime. Model bytes are
//! resolved only through `ModelManager`; ordinary transcription never downloads.

mod chinese_script;
mod diarization;

pub use diarization::{DiarizationModelResolver, LocalSpeakerDiarizer, OnlineCosineClustering};

use into_markdown_core::{
    AsrOptions, Block, BlockNode, BoxFuture, ChineseScript, ConversionError, ExecutionContext,
    ExecutionStage, Inline, MEDIA_CHECKPOINT_SCHEMA_VERSION, MediaCheckpoint, MediaCheckpointStage,
    NodeId, NormalizedAudioIdentity, Provenance, ProvenanceKind, ResourceReservation,
    SourceLocator, TimeRange, TimedToken, Transcriber, TranscriptionRequest, TranscriptionResult,
    estimate_retained_blocks,
};
use into_markdown_ffmpeg::{FfmpegRuntime, MediaLimits, NormalizedAudio};
use into_markdown_ocr::{ModelManager, ModelManagerError};
use std::path::Path;
#[cfg(all(target_os = "macos", feature = "metal"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

const PROVIDER_ID: &str = "builtin.asr.whisper-small";
const SAMPLE_RATE: u32 = 16_000;
const MAX_THREADS: u16 = 8;
const MAX_SEGMENTS: u32 = 100_000;
const MIN_NATIVE_MEMORY: u64 = 256 * 1024 * 1024;
const MAX_NATIVE_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024 * 1024;
const WINDOW_MS: u64 = 30_000;
const WINDOW_OVERLAP_MS: u64 = 2_000;
const SILENCE_SEARCH_MS: u64 = 2_000;
const SILENCE_PROBE_MS: u64 = 200;

/// Limits, output policy, and model selection for one installed service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperConfig {
    /// Embedded model bundle ID.
    pub model_bundle: String,
    /// Optional normalized Whisper language code.
    pub language: Option<String>,
    /// Deterministic Han-script output policy.
    pub chinese_script: ChineseScript,
    /// Maximum decoder threads.
    pub max_threads: u16,
    /// Optional maximum decoded duration.
    pub max_duration_ms: Option<u64>,
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
            chinese_script: options.chinese_script,
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
            || self.max_duration_ms == Some(0)
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
    #[cfg(all(target_os = "macos", feature = "metal"))]
    path: PathBuf,
    #[cfg(all(target_os = "macos", feature = "metal"))]
    backend: WhisperBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhisperBackend {
    Cpu,
    #[cfg(all(target_os = "macos", feature = "metal"))]
    Metal,
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
        // Native Whisper diagnostics must not corrupt CLI JSON stderr or Web
        // progress streams. With logging backends disabled this audited hook
        // is a process-wide no-op sink and is safe to install repeatedly.
        whisper_rs::install_logging_hooks();
        let (native, backend) = load_preferred_context(&artifact.path).map_err(|_| {
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
            #[cfg(all(target_os = "macos", feature = "metal"))]
            path: artifact.path,
            #[cfg(all(target_os = "macos", feature = "metal"))]
            backend,
        });
        #[cfg(not(all(target_os = "macos", feature = "metal")))]
        let _ = backend;
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
        context.report(ExecutionStage::Ai, Some(0), Some(1_000), Some("asr.normalize"))?;
        let pcm = self.ffmpeg.normalize_to_file_with_progress(
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
            "asr.normalize",
        )?;
        let duration_ms = pcm.frames.saturating_mul(1_000) / u64::from(SAMPLE_RATE);
        if self.config.max_duration_ms.is_some_and(|maximum| duration_ms > maximum) {
            return Err(resource("asrDuration"));
        }
        let mut cache = self.model(context)?;
        let model = cache.as_mut().ok_or_else(|| component(PROVIDER_ID, "model cache failed"))?;
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(usize::from(self.config.max_threads));
        let requested_language = request
            .language
            .map(normalize_language)
            .transpose()?
            .or_else(|| self.config.language.clone());
        let mut language = requested_language;
        let mut language_confidence = None;
        let mut output = Vec::new();
        output
            .try_reserve_exact(usize::try_from(self.config.max_segments.min(4_096)).unwrap_or(0))
            .map_err(|_| resource("asrSegments"))?;
        let mut transcript_bytes = 0_usize;
        let mut previous_end = 0_u64;
        let mut next_id = 1_u32;
        let total_frames = pcm.frames;
        let audio_identity = NormalizedAudioIdentity {
            sha256: pcm.sha256.clone(),
            frames: total_frames,
            sample_rate: pcm.sample_rate,
            channels: pcm.channels,
        };
        let window_frames = milliseconds_to_frames(WINDOW_MS)?;
        let overlap_frames = milliseconds_to_frames(WINDOW_OVERLAP_MS)?;
        let mut window_start = 0_u64;
        if let Some(recovered) = context.load_media_checkpoint()? {
            let checkpoint = recovered.into_state();
            if checkpoint.audio != audio_identity
                || checkpoint.transcriber_provider != PROVIDER_ID
                || checkpoint.transcriber_model != model.identity
            {
                return Err(ConversionError::Recovery {
                    reason: "incompatible",
                    detail: "normalized audio or ASR model changed after the media checkpoint"
                        .into(),
                });
            }
            output = checkpoint.segments;
            language = checkpoint.language;
            language_confidence = checkpoint.language_confidence;
            transcript_bytes = output
                .iter()
                .filter_map(|node| match &node.block {
                    Block::TimedSegment { content, .. } => Some(
                        content
                            .iter()
                            .filter_map(|inline| match inline {
                                Inline::Text { value, .. } => Some(value.len()),
                                _ => None,
                            })
                            .sum::<usize>(),
                    ),
                    _ => None,
                })
                .try_fold(0_usize, |total, bytes| total.checked_add(bytes))
                .filter(|total| *total <= MAX_TRANSCRIPT_BYTES)
                .ok_or_else(|| resource("asrTranscriptBytes"))?;
            previous_end = output
                .last()
                .and_then(|node| match node.block {
                    Block::TimedSegment { range, .. } => Some(range.end_ms),
                    _ => None,
                })
                .unwrap_or(0);
            next_id = u32::try_from(output.len())
                .ok()
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| resource("asrSegments"))?;
            window_start = match checkpoint.stage {
                MediaCheckpointStage::Transcribing => checkpoint.next_window_start_frame,
                MediaCheckpointStage::Diarizing => total_frames,
            };
        }
        while window_start < total_frames {
            context.checkpoint()?;
            let nominal_end = window_start.saturating_add(window_frames).min(total_frames);
            let window_end = if nominal_end < total_frames {
                choose_silence_boundary(&pcm, window_start, nominal_end, context)?
            } else {
                nominal_end
            };
            if window_end <= window_start {
                return Err(component(PROVIDER_ID, "long-form window made no progress"));
            }
            let samples = pcm_f32_range(&pcm, window_start, window_end, context)?;
            let decoded = decode_window(
                &model.context,
                &samples.values,
                threads,
                language.as_deref(),
                context,
                window_start,
                window_end,
                total_frames,
            );
            #[cfg(all(target_os = "macos", feature = "metal"))]
            let decoded = if decoded.is_err() && model.backend == WhisperBackend::Metal {
                // Native GPU availability can change after initialization. A
                // failed Metal window is retried once on a fresh CPU context;
                // subsequent windows stay on CPU and checkpoints remain valid.
                context.checkpoint()?;
                model.context = load_context(&model.path, false).map_err(|_| {
                    component(PROVIDER_ID, "Whisper CPU fallback could not be loaded")
                })?;
                model.backend = WhisperBackend::Cpu;
                decode_window(
                    &model.context,
                    &samples.values,
                    threads,
                    language.as_deref(),
                    context,
                    window_start,
                    window_end,
                    total_frames,
                )
            } else {
                decoded
            };
            let (state, detected) = decoded?;
            if let Some((detected_language, confidence)) = detected {
                language = Some(detected_language);
                language_confidence = confidence;
            }
            context.checkpoint()?;
            let ownership_start =
                if window_start == 0 { 0 } else { window_start.saturating_add(overlap_frames / 2) };
            let ownership_end = if window_end == total_frames {
                total_frames
            } else {
                window_end.saturating_sub(overlap_frames / 2)
            };
            let new_segments_start = output.len();
            collect_window_segments(
                &state,
                window_start,
                window_end,
                ownership_start,
                ownership_end,
                total_frames,
                self.config.max_segments,
                &model.identity,
                &mut output,
                &mut transcript_bytes,
                &mut previous_end,
                &mut next_id,
                context,
            )?;
            for segment in &mut output[new_segments_start..] {
                chinese_script::normalize_segment(segment, self.config.chinese_script);
            }
            let next_window_start = if window_end == total_frames {
                total_frames
            } else {
                window_end.saturating_sub(overlap_frames)
            };
            let _checkpoint_memory = context.reserve_memory(estimate_retained_blocks(&output)?)?;
            context.commit_media_checkpoint(&MediaCheckpoint {
                schema_version: MEDIA_CHECKPOINT_SCHEMA_VERSION,
                audio: audio_identity.clone(),
                stage: MediaCheckpointStage::Transcribing,
                next_window_start_frame: next_window_start,
                segments: output.clone(),
                transcriber_provider: PROVIDER_ID.into(),
                transcriber_model: model.identity.clone(),
                language: language.clone(),
                language_confidence,
                diarizer_provider: None,
                diarization_model: None,
                diarization_completed_segments: 0,
                speaker_clusters: Vec::new(),
            })?;
            window_start = next_window_start;
        }
        context.report(ExecutionStage::Ai, Some(650), Some(1_000), Some("asr.complete"))?;
        let output_language = match (language.as_deref(), self.config.chinese_script) {
            (Some("zh"), ChineseScript::Simplified) => Some("zh-Hans".into()),
            (Some("zh"), ChineseScript::Traditional) => Some("zh-Hant".into()),
            _ => language,
        };
        Ok(TranscriptionResult {
            segments: output,
            provider: PROVIDER_ID.into(),
            model: model.identity.clone(),
            language: output_language,
            language_confidence,
        })
    }
}

fn load_context(
    path: &Path,
    accelerated: bool,
) -> Result<WhisperContext, whisper_rs::WhisperError> {
    let mut parameters = WhisperContextParameters::default();
    // Metal is the acceleration boundary. Keep flash attention disabled: the
    // bundled whisper.cpp backend does not expose a recoverable Rust error for
    // every Metal flash-attention failure, so enabling it would bypass the CPU
    // fallback with a native process abort on affected macOS devices.
    parameters.use_gpu(accelerated).flash_attn(false);
    WhisperContext::new_with_params(path, parameters)
}

fn load_preferred_context(
    path: &Path,
) -> Result<(WhisperContext, WhisperBackend), whisper_rs::WhisperError> {
    #[cfg(all(target_os = "macos", feature = "metal"))]
    if process_allows_metal()
        && let Ok(context) = load_context(path, true)
    {
        return Ok((context, WhisperBackend::Metal));
    }
    load_context(path, false).map(|context| (context, WhisperBackend::Cpu))
}

#[cfg(all(target_os = "macos", feature = "metal"))]
fn process_allows_metal() -> bool {
    // App Sandbox and restricted local-agent sandboxes can expose a Metal
    // device while denying the private buffers whisper.cpp allocates during
    // model loading. Some native backends abort instead of returning an error
    // in that state, so choose the guaranteed CPU path before entering them.
    ["APP_SANDBOX_CONTAINER_ID", "SANDBOX_CONTAINER_ID", "CODEX_SANDBOX"]
        .iter()
        .all(|name| std::env::var_os(name).is_none())
}

#[allow(clippy::too_many_arguments)]
fn decode_window(
    native: &WhisperContext,
    samples: &[f32],
    threads: usize,
    requested_language: Option<&str>,
    context: &ExecutionContext,
    window_start: u64,
    window_end: u64,
    total_frames: u64,
) -> Result<(WhisperState, Option<(String, Option<f32>)>), ConversionError> {
    let mut state = native
        .create_state()
        .map_err(|_| component(PROVIDER_ID, "Whisper decoder state could not be created"))?;
    let detected = if requested_language.is_none() {
        state
            .pcm_to_mel(samples, threads)
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
        Some((language, confidence))
    } else {
        None
    };
    let language =
        requested_language.or_else(|| detected.as_ref().map(|(value, _)| value.as_str()));
    let mut params = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
    params.set_n_threads(i32::try_from(threads).map_err(|_| resource("asrConfiguration"))?);
    params.set_translate(false);
    params.set_no_timestamps(false);
    params.set_token_timestamps(true);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    params.set_language(language);
    install_window_callbacks(&mut params, context, window_start, window_end, total_frames);
    if state.full(params, samples).is_err() {
        context.checkpoint()?;
        return Err(component(PROVIDER_ID, "Whisper inference failed"));
    }
    Ok((state, detected))
}

#[cfg(test)]
fn install_callbacks(params: &mut FullParams<'_, '_>, context: &ExecutionContext) {
    install_window_callbacks(params, context, 0, 1, 1);
}

fn install_window_callbacks(
    params: &mut FullParams<'_, '_>,
    context: &ExecutionContext,
    window_start: u64,
    window_end: u64,
    total_frames: u64,
) {
    let abort_context = context.clone();
    params.set_abort_callback_safe(move || abort_context.checkpoint().is_err());
    let progress_context = context.clone();
    params.set_progress_callback_safe(move |progress: i32| {
        let local = u64::try_from(progress.clamp(0, 100)).unwrap_or_default();
        let completed = window_end
            .saturating_sub(window_start)
            .saturating_mul(local)
            .saturating_div(100)
            .saturating_add(window_start)
            .min(total_frames);
        let stage_units = completed.saturating_mul(650).saturating_div(total_frames.max(1));
        let _ = progress_context.report(
            ExecutionStage::Ai,
            Some(stage_units),
            Some(1_000),
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

struct TimedTokenBytes {
    range: TimeRange,
    bytes: Vec<u8>,
    confidence: Option<f32>,
}

fn decode_timed_tokens(tokens: Vec<TimedTokenBytes>) -> Result<Vec<TimedToken>, ConversionError> {
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut pending_range = None::<TimeRange>;
    let mut confidence_total = 0.0_f64;
    let mut confidence_count = 0_u32;
    for token in tokens {
        pending.try_reserve(token.bytes.len()).map_err(|_| resource("asrTranscriptBytes"))?;
        pending.extend_from_slice(&token.bytes);
        pending_range = Some(pending_range.map_or(token.range, |range| TimeRange {
            start_ms: range.start_ms,
            end_ms: token.range.end_ms,
        }));
        if let Some(confidence) = token.confidence {
            confidence_total += f64::from(confidence);
            confidence_count = confidence_count.saturating_add(1);
        }
        let Ok(text) = std::str::from_utf8(&pending) else { continue };
        output.try_reserve(1).map_err(|_| resource("asrSegments"))?;
        output.push(TimedToken {
            range: pending_range.take().ok_or_else(|| {
                component(PROVIDER_ID, "Whisper token timing buffer is inconsistent")
            })?,
            text: text.to_owned(),
            confidence: (confidence_count != 0)
                .then(|| (confidence_total / f64::from(confidence_count)) as f32),
            speaker: None,
            speaker_confidence: None,
        });
        pending.clear();
        confidence_total = 0.0;
        confidence_count = 0;
    }
    if !pending.is_empty() {
        return Err(component(PROVIDER_ID, "Whisper token text was not valid UTF-8"));
    }
    Ok(output)
}

fn pcm_f32_range(
    pcm: &NormalizedAudio,
    start_frame: u64,
    end_frame: u64,
    context: &ExecutionContext,
) -> Result<AccountedSamples, ConversionError> {
    if pcm.sample_rate != SAMPLE_RATE || pcm.channels != 1 {
        return Err(component(PROVIDER_ID, "FFmpeg returned an invalid PCM contract"));
    }
    if start_frame >= end_frame || end_frame > pcm.frames {
        return Err(component(PROVIDER_ID, "invalid long-form PCM window"));
    }
    let window = pcm.read_mono_s16le(start_frame, end_frame, context)?;
    let pcm_bytes = window.bytes();
    let sample_count = pcm_bytes.len() / 2;
    let bytes = u64::try_from(sample_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| resource("asrPcmSamples"))?;
    let memory = context.reserve_memory(bytes)?;
    let mut samples = Vec::new();
    samples.try_reserve_exact(sample_count).map_err(|_| resource("asrPcmSamples"))?;
    for (index, chunk) in pcm_bytes.chunks_exact(2).enumerate() {
        if index % 4_096 == 0 {
            context.checkpoint()?;
        }
        samples.push(f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32768.0);
    }
    Ok(AccountedSamples { values: samples, _memory: memory })
}

#[allow(clippy::too_many_arguments)]
fn collect_window_segments(
    state: &whisper_rs::WhisperState,
    window_start_frame: u64,
    window_end_frame: u64,
    ownership_start_frame: u64,
    ownership_end_frame: u64,
    total_frames: u64,
    maximum: u32,
    model: &str,
    output: &mut Vec<BlockNode>,
    transcript_bytes: &mut usize,
    previous_end: &mut u64,
    next_id: &mut u32,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let decoded_end_ms = frames_to_milliseconds(window_end_frame.min(total_frames));
    let count = u32::try_from(state.full_n_segments()).map_err(|_| resource("asrSegments"))?;
    if count > maximum || output.len().saturating_add(count as usize) > maximum as usize {
        return Err(resource("asrSegments"));
    }
    for segment in state.as_iter() {
        context.checkpoint()?;
        let offset_ms = frames_to_milliseconds(window_start_frame);
        let local_start_ms = u64::try_from(segment.start_timestamp())
            .ok()
            .and_then(|value| value.checked_mul(10))
            .ok_or_else(|| component(PROVIDER_ID, "invalid segment timestamp"))?;
        let local_end_ms = u64::try_from(segment.end_timestamp())
            .ok()
            .and_then(|value| value.checked_mul(10))
            .ok_or_else(|| component(PROVIDER_ID, "invalid segment timestamp"))?;
        if local_end_ms <= local_start_ms {
            return Err(component(PROVIDER_ID, "non-monotonic segment timestamp"));
        }
        let mut owned_bytes = Vec::new();
        let mut owned_start_ms = u64::MAX;
        let mut owned_end_ms = 0_u64;
        let mut probability_total = 0.0_f64;
        let mut probability_count = 0_u32;
        let mut saw_timed_token = false;
        let mut owned_token_bytes = Vec::new();
        for token_index in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(token_index) else { continue };
            let data = token.token_data();
            let (Ok(token_start), Ok(token_end)) = (u64::try_from(data.t0), u64::try_from(data.t1))
            else {
                continue;
            };
            if token_end <= token_start {
                continue;
            }
            saw_timed_token = true;
            let start_ms = offset_ms.saturating_add(token_start.saturating_mul(10));
            let end_ms = offset_ms.saturating_add(token_end.saturating_mul(10));
            let Some((start_ms, end_ms)) = clip_provider_range(start_ms, end_ms, decoded_end_ms)
            else {
                continue;
            };
            let midpoint_frame = milliseconds_to_frames(start_ms.saturating_add(end_ms) / 2)?;
            if !owns_timestamp(midpoint_frame, ownership_start_frame, ownership_end_frame) {
                continue;
            }
            let bytes = token
                .to_bytes()
                .map_err(|_| component(PROVIDER_ID, "Whisper returned invalid token text"))?;
            owned_bytes.try_reserve(bytes.len()).map_err(|_| resource("asrTranscriptBytes"))?;
            owned_bytes.extend_from_slice(bytes);
            owned_start_ms = owned_start_ms.min(start_ms);
            owned_end_ms = owned_end_ms.max(end_ms);
            let probability = token.token_probability();
            owned_token_bytes.try_reserve(1).map_err(|_| resource("asrSegments"))?;
            owned_token_bytes.push(TimedTokenBytes {
                range: TimeRange { start_ms, end_ms },
                bytes: bytes.to_vec(),
                confidence: probability.is_finite().then(|| probability.clamp(0.0, 1.0)),
            });
            if probability.is_finite() {
                probability_total += f64::from(probability.clamp(0.0, 1.0));
                probability_count = probability_count.saturating_add(1);
            }
        }
        let (text, mut start_ms, end_ms, confidence) = if saw_timed_token {
            if owned_bytes.is_empty()
                || owned_start_ms == u64::MAX
                || owned_end_ms <= owned_start_ms
            {
                continue;
            }
            let text = String::from_utf8(owned_bytes)
                .map_err(|_| component(PROVIDER_ID, "Whisper token text was not valid UTF-8"))?;
            let confidence = if probability_count == 0 {
                0.0
            } else {
                (probability_total / f64::from(probability_count)) as f32
            };
            (text, owned_start_ms, owned_end_ms, confidence)
        } else {
            // Older compatible Whisper builds may omit token timing despite the
            // requested flag. Preserve the segment exactly once by the same
            // half-open ownership rule instead of duplicating it across windows.
            let start_ms = offset_ms.saturating_add(local_start_ms);
            let end_ms = offset_ms.saturating_add(local_end_ms);
            let Some((start_ms, end_ms)) = clip_provider_range(start_ms, end_ms, decoded_end_ms)
            else {
                continue;
            };
            let midpoint_frame = milliseconds_to_frames(start_ms.saturating_add(end_ms) / 2)?;
            if !owns_timestamp(midpoint_frame, ownership_start_frame, ownership_end_frame) {
                continue;
            }
            let text = segment
                .to_str()
                .map_err(|_| component(PROVIDER_ID, "Whisper returned invalid segment text"))?
                .to_owned();
            (text, start_ms, end_ms, segment_confidence(&segment))
        };
        if text.is_empty() {
            continue;
        }
        let mut owned_tokens = decode_timed_tokens(owned_token_bytes)?;
        start_ms = start_ms.max(*previous_end);
        if end_ms <= start_ms {
            continue;
        }
        let mut token_end = start_ms;
        owned_tokens.retain_mut(|token| {
            token.range.start_ms = token.range.start_ms.max(start_ms).max(token_end);
            token.range.end_ms = token.range.end_ms.min(end_ms);
            if token.range.end_ms <= token.range.start_ms {
                return false;
            }
            token_end = token.range.end_ms;
            true
        });
        if !owned_tokens.is_empty()
            && !owned_tokens.iter().flat_map(|token| token.text.bytes()).eq(text.bytes())
        {
            // Preserve the exact transcript when a provider timestamp anomaly
            // forces a token outside the monotonic segment range. Without
            // reliable token evidence diarization falls back to the whole
            // segment instead of rewriting or inventing text.
            owned_tokens.clear();
        }
        *transcript_bytes = transcript_bytes
            .checked_add(text.len())
            .filter(|total| *total <= MAX_TRANSCRIPT_BYTES)
            .ok_or_else(|| resource("asrTranscriptBytes"))?;
        *previous_end = end_ms;
        let range = TimeRange { start_ms, end_ms };
        output.push(BlockNode {
            id: NodeId(format!("asr-segment-{next_id:06}")),
            block: Block::TimedSegment {
                range,
                speaker: None,
                speaker_confidence: None,
                tokens: owned_tokens,
                content: vec![Inline::Text { value: text, marks: Vec::new() }],
            },
            provenance: Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: format!("{PROVIDER_ID}/{model}"),
                locator: SourceLocator { time: Some(range), ..SourceLocator::default() },
                confidence: Some(confidence),
            },
        });
        *next_id = next_id.checked_add(1).ok_or_else(|| resource("asrSegments"))?;
    }
    Ok(())
}

fn clip_provider_range(start_ms: u64, end_ms: u64, decoded_end_ms: u64) -> Option<(u64, u64)> {
    let end_ms = end_ms.min(decoded_end_ms);
    (start_ms < end_ms).then_some((start_ms, end_ms))
}

fn owns_timestamp(
    midpoint_frame: u64,
    ownership_start_frame: u64,
    ownership_end_frame: u64,
) -> bool {
    midpoint_frame >= ownership_start_frame && midpoint_frame < ownership_end_frame
}

fn milliseconds_to_frames(milliseconds: u64) -> Result<u64, ConversionError> {
    milliseconds
        .checked_mul(u64::from(SAMPLE_RATE))
        .and_then(|value| value.checked_add(999))
        .map(|value| value / 1_000)
        .ok_or_else(|| resource("asrDuration"))
}

fn frames_to_milliseconds(frames: u64) -> u64 {
    frames.saturating_mul(1_000) / u64::from(SAMPLE_RATE)
}

fn choose_silence_boundary(
    pcm: &NormalizedAudio,
    window_start: u64,
    nominal_end: u64,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let search = milliseconds_to_frames(SILENCE_SEARCH_MS)?;
    let probe = milliseconds_to_frames(SILENCE_PROBE_MS)?;
    let earliest = nominal_end.saturating_sub(search).max(window_start.saturating_add(probe));
    let latest = nominal_end.saturating_sub(probe);
    if earliest >= latest {
        return Ok(nominal_end);
    }
    let window = pcm.read_mono_s16le(earliest, nominal_end, context)?;
    let window_bytes = window.bytes();
    let stride = (probe / 2).max(1);
    let mut best = nominal_end;
    let mut best_energy = u128::MAX;
    let mut candidate = earliest;
    while candidate <= latest {
        context.checkpoint()?;
        let start = usize::try_from(candidate.saturating_sub(earliest).saturating_mul(2))
            .map_err(|_| resource("asrPcmSamples"))?;
        let end_frame = candidate.saturating_add(probe).min(nominal_end);
        let end = usize::try_from(end_frame.saturating_sub(earliest).saturating_mul(2))
            .map_err(|_| resource("asrPcmSamples"))?;
        let bytes = window_bytes.get(start..end).ok_or_else(|| resource("asrPcmSamples"))?;
        let mut energy = 0_u128;
        for sample in bytes.chunks_exact(2) {
            let value = i64::from(i16::from_le_bytes([sample[0], sample[1]]));
            energy = energy.saturating_add(u128::from(value.unsigned_abs()).pow(2));
        }
        if energy < best_energy {
            best_energy = energy;
            best = end_frame;
        }
        candidate = candidate.saturating_add(stride);
    }
    Ok(best.min(nominal_end))
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
        options.max_duration_ms = Some(0);
        assert!(WhisperConfig::try_from(&options).is_err());
        options.max_duration_ms = None;
        options.max_native_memory_bytes = MIN_NATIVE_MEMORY - 1;
        assert!(WhisperConfig::try_from(&options).is_err());
    }

    #[test]
    fn overlapping_windows_have_one_half_open_token_owner() {
        let boundary = milliseconds_to_frames(29_000).unwrap();
        assert!(owns_timestamp(boundary - 1, 0, boundary));
        assert!(!owns_timestamp(boundary, 0, boundary));
        assert!(owns_timestamp(boundary, boundary, milliseconds_to_frames(57_000).unwrap()));
        assert!(!owns_timestamp(
            milliseconds_to_frames(57_000).unwrap(),
            boundary,
            milliseconds_to_frames(57_000).unwrap(),
        ));
    }

    #[test]
    fn provider_ranges_are_clipped_to_the_decoded_window() {
        assert_eq!(clip_provider_range(54_900, 55_600, 55_000), Some((54_900, 55_000)));
        assert_eq!(clip_provider_range(55_000, 55_600, 55_000), None);
        assert_eq!(clip_provider_range(2_000, 1_000, 55_000), None);
    }

    #[test]
    fn token_bytes_split_inside_utf8_are_rejoined_without_text_rewrite() {
        let bytes = "张".as_bytes();
        let tokens = decode_timed_tokens(vec![
            TimedTokenBytes {
                range: TimeRange { start_ms: 0, end_ms: 10 },
                bytes: bytes[..1].to_vec(),
                confidence: Some(0.8),
            },
            TimedTokenBytes {
                range: TimeRange { start_ms: 10, end_ms: 30 },
                bytes: bytes[1..].to_vec(),
                confidence: Some(1.0),
            },
        ])
        .unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].text, "张");
        assert_eq!(tokens[0].range, TimeRange { start_ms: 0, end_ms: 30 });
        assert_eq!(tokens[0].confidence, Some(0.9));
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
