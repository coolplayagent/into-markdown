//! Typed, policy-bound OpenAI-compatible OCR and transcription adapters.

use crate::{
    GenerationEndpoint, GenerationInput, GenerationRequest, OpenAiCompatibleClient, ProviderError,
    ProviderErrorCode, ProviderNetworkPolicy,
};
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, ExecutionContext, Inline,
    NodeId, OcrEngine, OcrOutputPlan, OcrRecognition, OcrRegion, OcrRequest, OcrResult, Provenance,
    ProvenanceKind, SourceLocator, TimeRange, Transcriber, TranscriptionRequest,
    TranscriptionResult,
};

const OCR_PROMPT: &str =
    "Extract all visible text in reading order. Return text only, without commentary.";
const OCR_MAX_OUTPUT_TOKENS: u32 = 16_384;
const ASR_MAX_OUTPUT_TOKENS: u32 = 16_384;
const MAX_REMOTE_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REMOTE_AUDIO_BYTES: usize = 10 * 1024 * 1024;
const MAX_REMOTE_AUDIO_DURATION_MS: u64 = 300_000;
const MAX_REMOTE_TEXT_BYTES: u64 = 256 * 1024;
const FIXED_WORKING_BYTES: u64 = 1024 * 1024;

/// OpenAI-compatible remote OCR exposed through the same typed OCR contract as local plugins.
pub struct OpenAiRemoteOcr {
    client: OpenAiCompatibleClient,
    network: ProviderNetworkPolicy,
    provider_id: String,
}

impl std::fmt::Debug for OpenAiRemoteOcr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiRemoteOcr")
            .field("client", &self.client)
            .field("network", &self.network)
            .field("provider_id", &self.provider_id)
            .finish()
    }
}

impl OpenAiRemoteOcr {
    /// Bind a validated client to a stable, non-secret provenance identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing provider identities.
    pub fn new(
        client: OpenAiCompatibleClient,
        network: ProviderNetworkPolicy,
        provider_id: impl Into<String>,
    ) -> Result<Self, ConversionError> {
        let provider_id = provider_id.into();
        validate_provider_id(&provider_id)?;
        Ok(Self { client, network, provider_id })
    }
}

impl OcrEngine for OpenAiRemoteOcr {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn provenance_kind(&self) -> ProvenanceKind {
        ProvenanceKind::AiProvider
    }

    fn planned_bound_output(
        &self,
        request: OcrRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        validate_network(options, &self.network)?;
        if request.image.is_empty()
            || request.image.len() > MAX_REMOTE_IMAGE_BYTES
            || !matches!(
                request.media_type,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        {
            return Err(ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "remote OCR image is empty, oversized, or unsupported".into(),
            });
        }
        let text_bytes = options.limits.max_field_bytes.clamp(1, MAX_REMOTE_TEXT_BYTES);
        let retained = text_bytes.checked_add(16 * 1024).ok_or_else(memory_overflow)?;
        let working = u64::try_from(request.image.len())
            .map_err(|_| memory_overflow())?
            .checked_mul(5)
            .and_then(|bytes| bytes.checked_add(FIXED_WORKING_BYTES))
            .ok_or_else(memory_overflow)?;
        let total = retained.checked_add(working).ok_or_else(memory_overflow)?;
        if total > options.limits.max_memory_bytes || total > context.available_memory_bytes() {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "remote OCR request exceeds the available memory budget".into(),
            });
        }
        OcrOutputPlan::try_new_with_working(retained, working, 1, text_bytes)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        validate_network(options, &self.network)?;
        if width == 0 || height == 0 {
            return Err(ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "remote OCR normalized image dimensions must be non-zero".into(),
            });
        }
        let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or_else(memory_overflow)?;
        let decoded = pixels.checked_mul(4).ok_or_else(memory_overflow)?;
        let encoded = u64::try_from(MAX_REMOTE_IMAGE_BYTES).map_err(|_| memory_overflow())?;
        let text_bytes = options.limits.max_field_bytes.clamp(1, MAX_REMOTE_TEXT_BYTES);
        let retained = text_bytes.checked_add(16 * 1024).ok_or_else(memory_overflow)?;
        let working = decoded
            .checked_add(encoded.checked_mul(5).ok_or_else(memory_overflow)?)
            .and_then(|bytes| bytes.checked_add(FIXED_WORKING_BYTES))
            .ok_or_else(memory_overflow)?;
        let total = retained.checked_add(working).ok_or_else(memory_overflow)?;
        if total > options.limits.max_memory_bytes || total > context.available_memory_bytes() {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "remote OCR normalized image exceeds the available memory budget".into(),
            });
        }
        OcrOutputPlan::try_new_with_working(retained, working, 1, text_bytes)
    }

    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let result = self
                .client
                .generate(
                    GenerationRequest {
                        endpoint: GenerationEndpoint::ChatCompletions,
                        capability: "vision-ocr",
                        input: GenerationInput::Image {
                            bytes: request.image,
                            media_type: request.media_type,
                            prompt: OCR_PROMPT,
                        },
                        max_output_tokens: OCR_MAX_OUTPUT_TOKENS,
                        idempotency_key: None,
                    },
                    context,
                )
                .map_err(|error| map_provider_error(&self.provider_id, &error))?;
            let text = result.text.trim();
            if text.is_empty()
                || u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_REMOTE_TEXT_BYTES
            {
                return Err(ConversionError::Ocr {
                    provider: self.provider_id.clone(),
                    detail: "remote OCR returned empty or oversized text".into(),
                });
            }
            Ok(OcrResult {
                regions: vec![OcrRegion {
                    text: text.into(),
                    // Geometry and confidence are deliberately not invented.
                    // The remote result remains `OcrRecognition::Unbound` and
                    // is materialized as page-scoped AI text by the converter.
                    polygon: [(0.0, 0.0); 4],
                    confidence: 0.0,
                }],
                provider: self.provider_id.clone(),
            })
        })
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move { self.recognize(request, context).await.map(OcrRecognition::Remote) })
    }
}

/// OpenAI-compatible remote transcription exposed through the typed media contract.
pub struct OpenAiRemoteTranscriber {
    client: OpenAiCompatibleClient,
    provider_id: String,
    model_id: String,
}

impl std::fmt::Debug for OpenAiRemoteTranscriber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiRemoteTranscriber")
            .field("client", &self.client)
            .field("provider_id", &self.provider_id)
            .field("model_id", &self.model_id)
            .finish()
    }
}

impl OpenAiRemoteTranscriber {
    /// Bind a validated client to exact provider and model identities.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing identities.
    pub fn new(
        client: OpenAiCompatibleClient,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<Self, ConversionError> {
        let provider_id = provider_id.into();
        let model_id = model_id.into();
        validate_provider_id(&provider_id)?;
        if model_id.is_empty() || model_id.len() > 512 || model_id.chars().any(char::is_control) {
            return Err(ConversionError::ComponentUnavailable {
                component: "remote-transcription".into(),
                detail: "remote transcription model identity is invalid".into(),
            });
        }
        Ok(Self { client, provider_id, model_id })
    }
}

impl Transcriber for OpenAiRemoteTranscriber {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn allows_prefer_fallback(&self, error: &ConversionError) -> bool {
        matches!(
            error,
            ConversionError::ResourceLimit {
                limit: "max_input_bytes" | "max_input_duration_ms",
                ..
            }
        )
    }

    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if request.media.is_empty() || request.media.len() > MAX_REMOTE_AUDIO_BYTES {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_input_bytes",
                    detail: "remote transcription media is empty or exceeds 10 MiB".into(),
                });
            }
            let media_type = canonical_remote_audio_media_type(request.media_type, request.media)
                .ok_or_else(|| ConversionError::Unsupported {
                    detail: "remote transcription requires WAV, MP3, M4A/MP4, AAC, FLAC, Ogg/Opus, or WebM media"
                        .into(),
                })?;
            let duration_ms =
                remote_audio_duration_ms(media_type, request.media).ok_or_else(|| {
                    ConversionError::Unsupported {
                        detail:
                            "remote transcription could not verify the media duration before upload"
                                .into(),
                    }
                })?;
            if duration_ms > MAX_REMOTE_AUDIO_DURATION_MS {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_input_duration_ms",
                    detail: "remote transcription media exceeds the 5-minute provider limit".into(),
                });
            }
            let result = self
                .client
                .generate(
                    GenerationRequest {
                        endpoint: GenerationEndpoint::ChatCompletions,
                        capability: "audio-transcription",
                        input: GenerationInput::Audio {
                            bytes: request.media,
                            media_type,
                            language: request.language,
                        },
                        max_output_tokens: ASR_MAX_OUTPUT_TOKENS,
                        idempotency_key: None,
                    },
                    context,
                )
                .map_err(|error| map_provider_error(&self.provider_id, &error))?;
            let text = result.text.trim();
            let audio = result.audio.ok_or_else(|| ConversionError::Ai {
                provider: self.provider_id.clone(),
                detail: "remote transcription response omitted bounded audio metadata".into(),
            })?;
            if text.is_empty()
                || u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_REMOTE_TEXT_BYTES
            {
                return Err(ConversionError::Ai {
                    provider: self.provider_id.clone(),
                    detail: "remote transcription returned empty or oversized text".into(),
                });
            }
            let range = TimeRange { start_ms: 0, end_ms: audio.duration_ms };
            let segment = BlockNode {
                id: NodeId("remote-transcript-0".into()),
                block: Block::TimedSegment {
                    range,
                    speaker: None,
                    speaker_confidence: None,
                    tokens: Vec::new(),
                    content: vec![Inline::Text { value: text.into(), marks: Vec::new() }],
                },
                provenance: Provenance {
                    kind: ProvenanceKind::AiProvider,
                    provider: self.provider_id.clone(),
                    locator: SourceLocator { time: Some(range), ..SourceLocator::default() },
                    confidence: None,
                },
            };
            Ok(TranscriptionResult {
                segments: vec![segment],
                provider: self.provider_id.clone(),
                model: self.model_id.clone(),
                language: audio.language.or_else(|| request.language.map(str::to_owned)),
                language_confidence: None,
            })
        })
    }
}

fn canonical_remote_audio_media_type<'a>(declared: &'a str, bytes: &[u8]) -> Option<&'a str> {
    let normalized = match declared {
        "audio/x-wav" | "audio/vnd.wave" => Some("audio/wav"),
        "audio/mp3" => Some("audio/mpeg"),
        "audio/x-m4a" => Some("audio/m4a"),
        "application/ogg" => Some("audio/ogg"),
        value
            if matches!(
                value,
                "audio/aac"
                    | "audio/flac"
                    | "audio/m4a"
                    | "audio/mp4"
                    | "audio/mpeg"
                    | "audio/ogg"
                    | "audio/opus"
                    | "audio/wav"
                    | "audio/webm"
                    | "video/mp4"
                    | "video/webm"
            ) =>
        {
            Some(value)
        }
        _ => None,
    };
    normalized.or_else(|| sniff_remote_audio_media_type(bytes))
}

fn sniff_remote_audio_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0 {
        Some("audio/aac")
    } else if bytes.starts_with(b"ID3")
        || bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0
    {
        Some("audio/mpeg")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("audio/mp4")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some("audio/webm")
    } else {
        None
    }
}

fn remote_audio_duration_ms(media_type: &str, bytes: &[u8]) -> Option<u64> {
    match media_type {
        "audio/wav" => wav_duration_ms(bytes),
        "audio/mpeg" => mp3_duration_ms(bytes),
        "audio/m4a" | "audio/mp4" | "video/mp4" => mp4_duration_ms(bytes),
        "audio/webm" | "video/webm" => webm_duration_ms(bytes),
        "audio/ogg" | "audio/opus" => ogg_duration_ms(bytes),
        "audio/flac" => flac_duration_ms(bytes),
        "audio/aac" => aac_duration_ms(bytes),
        _ => None,
    }
}

fn duration_ms(samples: u64, sample_rate: u64) -> Option<u64> {
    (sample_rate != 0)
        .then_some(samples)
        .and_then(|samples| samples.checked_mul(1_000))
        .map(|milliseconds| milliseconds / sample_rate)
}

fn wav_duration_ms(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12_usize;
    let mut bytes_per_second = None;
    let mut data_bytes = None;
    while offset.checked_add(8)? <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let length =
            usize::try_from(u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?))
                .ok()?;
        let start = offset.checked_add(8)?;
        let end = start.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if id == b"fmt " && length >= 12 {
            let rate = u32::from_le_bytes(bytes[start + 8..start + 12].try_into().ok()?);
            if rate != 0 {
                bytes_per_second = Some(u64::from(rate));
            }
        } else if id == b"data" {
            data_bytes = Some(u64::try_from(length).ok()?);
        }
        offset = end.checked_add(length % 2)?;
    }
    duration_ms(data_bytes?, bytes_per_second?)
}

fn mp4_duration_ms(bytes: &[u8]) -> Option<u64> {
    fn find_mvhd(bytes: &[u8], mut offset: usize, end: usize, depth: u8) -> Option<(u64, u64)> {
        while offset.checked_add(8)? <= end {
            let mut size =
                u64::from(u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?));
            let kind = &bytes[offset + 4..offset + 8];
            let mut header = 8_usize;
            if size == 1 {
                size = u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().ok()?);
                header = 16;
            } else if size == 0 {
                size = u64::try_from(end.checked_sub(offset)?).ok()?;
            }
            let size = usize::try_from(size).ok()?;
            if size < header || offset.checked_add(size)? > end {
                return None;
            }
            let payload = offset.checked_add(header)?;
            let atom_end = offset.checked_add(size)?;
            if kind == b"mvhd" {
                let version = *bytes.get(payload)?;
                let (rate_offset, duration_offset, duration_bytes) = if version == 0 {
                    (12, 16, 4)
                } else if version == 1 {
                    (20, 24, 8)
                } else {
                    return None;
                };
                let rate_start = payload.checked_add(rate_offset)?;
                let duration_start = payload.checked_add(duration_offset)?;
                let rate = u64::from(u32::from_be_bytes(
                    bytes.get(rate_start..rate_start + 4)?.try_into().ok()?,
                ));
                let duration = if duration_bytes == 4 {
                    u64::from(u32::from_be_bytes(
                        bytes.get(duration_start..duration_start + 4)?.try_into().ok()?,
                    ))
                } else {
                    u64::from_be_bytes(
                        bytes.get(duration_start..duration_start + 8)?.try_into().ok()?,
                    )
                };
                return Some((duration, rate));
            }
            if depth < 4
                && matches!(kind, b"moov" | b"trak" | b"mdia")
                && let Some(found) = find_mvhd(bytes, payload, atom_end, depth + 1)
            {
                return Some(found);
            }
            offset = atom_end;
        }
        None
    }
    let (duration, timescale) = find_mvhd(bytes, 0, bytes.len(), 0)?;
    duration_ms(duration, timescale)
}

fn webm_duration_ms(bytes: &[u8]) -> Option<u64> {
    fn vint(bytes: &[u8], offset: usize, keep_marker: bool) -> Option<(u64, usize)> {
        let first = *bytes.get(offset)?;
        let length = (first.leading_zeros() as usize).checked_add(1)?;
        if length > 8 || offset.checked_add(length)? > bytes.len() {
            return None;
        }
        let marker = 1_u8.checked_shl(u32::try_from(8_usize.checked_sub(length)?).ok()?)?;
        let mut value = if keep_marker { u64::from(first) } else { u64::from(first & !marker) };
        for byte in &bytes[offset + 1..offset + length] {
            value = value.checked_shl(8)?.checked_add(u64::from(*byte))?;
        }
        Some((value, length))
    }
    let mut offset = 0_usize;
    let mut scale = 1_000_000_u64;
    let mut duration = None;
    while offset < bytes.len().min(1024 * 1024) {
        let Some((id, id_len)) = vint(bytes, offset, true) else { break };
        let size_offset = offset.checked_add(id_len)?;
        let Some((size, size_len)) = vint(bytes, size_offset, false) else { break };
        let payload = size_offset.checked_add(size_len)?;
        let size = usize::try_from(size).ok()?;
        let end = payload.checked_add(size)?;
        if end > bytes.len() {
            offset = offset.checked_add(1)?;
            continue;
        }
        if id == 0x2a_d7_b1 && (1..=8).contains(&size) {
            scale = bytes[payload..end]
                .iter()
                .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        } else if id == 0x44_89 {
            duration = match size {
                4 => Some(f64::from(f32::from_bits(u32::from_be_bytes(
                    bytes[payload..end].try_into().ok()?,
                )))),
                8 => Some(f64::from_bits(u64::from_be_bytes(bytes[payload..end].try_into().ok()?))),
                _ => None,
            };
        }
        if duration.is_some() {
            break;
        }
        offset = if matches!(id, 0x1a45_dfa3 | 0x1853_8067 | 0x1549_a966) { payload } else { end };
    }
    let duration = duration.filter(|value| value.is_finite() && *value >= 0.0)?;
    let seconds = duration * f64::from(u32::try_from(scale).ok()?) / 1_000_000_000.0;
    let milliseconds = std::time::Duration::try_from_secs_f64(seconds).ok()?.as_millis();
    u64::try_from(milliseconds).ok()
}

fn ogg_duration_ms(bytes: &[u8]) -> Option<u64> {
    let rate = if bytes.windows(8).any(|window| window == b"OpusHead") {
        48_000_u64
    } else {
        let marker = bytes.windows(7).position(|window| window == b"\x01vorbis")?;
        let start = marker.checked_add(12)?;
        u64::from(u32::from_le_bytes(bytes.get(start..start + 4)?.try_into().ok()?))
    };
    let mut offset = 0_usize;
    let mut granule = None;
    while let Some(relative) = bytes.get(offset..)?.windows(4).position(|window| window == b"OggS")
    {
        let page = offset.checked_add(relative)?;
        let candidate = u64::from_le_bytes(bytes.get(page + 6..page + 14)?.try_into().ok()?);
        if candidate != u64::MAX {
            granule = Some(candidate);
        }
        let segments = usize::from(*bytes.get(page + 26)?);
        let table = bytes.get(page + 27..page + 27 + segments)?;
        let payload = table.iter().map(|value| usize::from(*value)).sum::<usize>();
        offset = page.checked_add(27)?.checked_add(segments)?.checked_add(payload)?;
    }
    duration_ms(granule?, rate)
}

fn flac_duration_ms(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 42 || !bytes.starts_with(b"fLaC") || bytes[4] & 0x7f != 0 {
        return None;
    }
    let length =
        (usize::from(bytes[5]) << 16) | (usize::from(bytes[6]) << 8) | usize::from(bytes[7]);
    if length < 34 || bytes.len() < 8 + length {
        return None;
    }
    let packed = u64::from_be_bytes(bytes[18..26].try_into().ok()?);
    let rate = (packed >> 44) & 0x0f_ffff;
    let samples = packed & 0x0f_ffff_ffff;
    duration_ms(samples, rate)
}

fn aac_duration_ms(bytes: &[u8]) -> Option<u64> {
    const RATES: [u64; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut offset = 0_usize;
    let mut samples = 0_u64;
    let mut rate = None;
    while offset + 7 <= bytes.len() {
        if bytes[offset] != 0xff || bytes[offset + 1] & 0xf6 != 0xf0 {
            return None;
        }
        let index = usize::from((bytes[offset + 2] >> 2) & 0x0f);
        let current_rate = *RATES.get(index)?;
        if rate.replace(current_rate).is_some_and(|previous| previous != current_rate) {
            return None;
        }
        let frame_length = (usize::from(bytes[offset + 3] & 0x03) << 11)
            | (usize::from(bytes[offset + 4]) << 3)
            | usize::from(bytes[offset + 5] >> 5);
        if frame_length < 7 || offset.checked_add(frame_length)? > bytes.len() {
            return None;
        }
        samples = samples.checked_add(1_024 * u64::from((bytes[offset + 6] & 0x03) + 1))?;
        offset = offset.checked_add(frame_length)?;
    }
    (offset == bytes.len()).then_some(()).and_then(|()| duration_ms(samples, rate?))
}

fn mp3_duration_ms(bytes: &[u8]) -> Option<u64> {
    const V1_L1: [u16; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const V1_L2: [u16; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V1_L3: [u16; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2_L1: [u16; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const V2_L23: [u16; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let mut offset = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = bytes[6..10].iter().try_fold(0_usize, |value, byte| {
            ((*byte & 0x80) == 0).then_some((value << 7) | usize::from(*byte))
        })?;
        10_usize.checked_add(size)?
    } else {
        0
    };
    let mut total_samples = 0_u64;
    let mut sample_rate = None;
    let mut frames = 0_u64;
    while offset + 4 <= bytes.len() {
        let header = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?);
        if header >> 21 != 0x7ff {
            if frames == 0 && offset < 64 * 1024 {
                offset += 1;
                continue;
            }
            break;
        }
        let version = (header >> 19) & 0x03;
        let layer = (header >> 17) & 0x03;
        let bitrate_index = usize::try_from((header >> 12) & 0x0f).ok()?;
        let rate_index = usize::try_from((header >> 10) & 0x03).ok()?;
        if version == 1 || layer == 0 || !(1..=14).contains(&bitrate_index) || rate_index == 3 {
            return None;
        }
        let v1 = version == 3;
        let base_rate = [44_100_u64, 48_000, 32_000][rate_index];
        let rate = match version {
            3 => base_rate,
            2 => base_rate / 2,
            0 => base_rate / 4,
            _ => return None,
        };
        if sample_rate.replace(rate).is_some_and(|previous| previous != rate) {
            return None;
        }
        let bitrate = u64::from(match (v1, layer) {
            (true, 3) => V1_L1[bitrate_index],
            (true, 2) => V1_L2[bitrate_index],
            (true, 1) => V1_L3[bitrate_index],
            (false, 3) => V2_L1[bitrate_index],
            (false, 2 | 1) => V2_L23[bitrate_index],
            _ => return None,
        }) * 1_000;
        let padding = u64::from((header >> 9) & 1);
        let (samples, frame_length) = if layer == 3 {
            (384_u64, ((12 * bitrate / rate) + padding) * 4)
        } else if layer == 1 && !v1 {
            (576_u64, 72 * bitrate / rate + padding)
        } else {
            (1_152_u64, 144 * bitrate / rate + padding)
        };
        let frame_length = usize::try_from(frame_length).ok()?;
        if frame_length < 4 || offset.checked_add(frame_length)? > bytes.len() {
            break;
        }
        total_samples = total_samples.checked_add(samples)?;
        frames += 1;
        offset = offset.checked_add(frame_length)?;
    }
    (frames != 0).then_some(()).and_then(|()| duration_ms(total_samples, sample_rate?))
}

fn validate_provider_id(value: &str) -> Result<(), ConversionError> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_control)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(ConversionError::ComponentUnavailable {
            component: "remote-provider".into(),
            detail: "remote provider identity is invalid".into(),
        });
    }
    Ok(())
}

fn validate_network(
    options: &ConversionOptions,
    actual: &ProviderNetworkPolicy,
) -> Result<(), ConversionError> {
    let expected = ProviderNetworkPolicy {
        allow_network: options.network.enabled,
        allow_private_network: !options.network.deny_private_networks,
        allowed_hosts: options.network.allowed_hosts.clone(),
    };
    if &expected != actual {
        return Err(ConversionError::Network {
            detail: "remote capability network policy does not match this invocation".into(),
        });
    }
    Ok(())
}

fn map_provider_error(provider: &str, error: &ProviderError) -> ConversionError {
    match error.code() {
        ProviderErrorCode::Cancelled => ConversionError::Cancelled,
        ProviderErrorCode::Timeout => ConversionError::Timeout,
        ProviderErrorCode::ResourceLimit | ProviderErrorCode::ResponseTooLarge => {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: error.code_str().into(),
            }
        }
        ProviderErrorCode::NetworkDenied
        | ProviderErrorCode::HostDenied
        | ProviderErrorCode::PrivateNetworkDenied
        | ProviderErrorCode::Dns
        | ProviderErrorCode::Connect
        | ProviderErrorCode::Tls
        | ProviderErrorCode::RedirectDenied => {
            ConversionError::Network { detail: error.code_str().into() }
        }
        _ => ConversionError::Ai { provider: provider.into(), detail: error.code_str().into() },
    }
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "remote capability memory plan overflow".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderConfig;
    use into_markdown_core::{
        Block, ExecutionOptions, OcrRecognition, ResourceLimits, TranscriptionRequest,
    };
    use std::future::Future;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    fn wav_fixture(data_bytes: usize, bytes_per_second: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(44 + data_bytes);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&u32::try_from(36 + data_bytes).unwrap().to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&bytes_per_second.to_le_bytes());
        bytes.extend_from_slice(&bytes_per_second.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&u32::try_from(data_bytes).unwrap().to_le_bytes());
        bytes.resize(44 + data_bytes, 0);
        bytes
    }
    use std::time::Duration;

    fn context() -> ExecutionContext {
        ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_secs(2)),
                ..ExecutionOptions::default()
            },
            ResourceLimits { max_memory_bytes: 64 * 1024 * 1024, ..ResourceLimits::default() },
        )
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(value) => return value,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn client(
        address: std::net::SocketAddr,
        capability: &str,
        network: bool,
        secret_environment: &str,
    ) -> (OpenAiCompatibleClient, ProviderNetworkPolicy) {
        let config = ProviderConfig::parse(
            &format!("http://{address}/v1"),
            if capability == "audio-transcription" { "qwen3-asr-flash" } else { "qwen3.5-ocr" },
            secret_environment,
            Duration::from_secs(2),
            [capability.into()],
        )
        .unwrap();
        let policy = ProviderNetworkPolicy {
            allow_network: network,
            allow_private_network: true,
            allowed_hosts: vec!["127.0.0.1".into()],
        };
        (OpenAiCompatibleClient::new(config, policy.clone()), policy)
    }

    fn controlled_test_secret_environment() -> &'static str {
        [
            "CODEX_SESSION_ID",
            "GITHUB_SHA",
            "GITHUB_RUN_ID",
            "COMPUTERNAME",
            "HOSTNAME",
            "USERDOMAIN",
        ]
        .into_iter()
        .find(|name| {
            std::env::var(name).is_ok_and(|value| {
                !value.is_empty()
                    && value.len() <= 4096
                    && !value.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
            })
        })
        .expect("a non-secret platform process marker is required")
    }

    fn serve(body: &'static [u8]) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let header_end = request.len();
            let headers = std::str::from_utf8(&request).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                })
                .unwrap()
                .parse::<usize>()
                .unwrap();
            request.resize(header_end + length, 0);
            stream.read_exact(&mut request[header_end..]).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
            request
        });
        (address, worker)
    }

    #[test]
    fn remote_ocr_returns_typed_unbound_text_with_exact_provider_identity() {
        let response = br#"{"id":"chatcmpl-ocr","object":"chat.completion","created":1,"model":"qwen3.5-ocr","choices":[{"index":0,"message":{"role":"assistant","content":"\u53d1\u7968\u53f7\u7801 12345","annotations":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":4,"total_tokens":5}}"#;
        let (address, worker) = serve(response);
        let (client, policy) =
            client(address, "vision-ocr", true, controlled_test_secret_environment());
        let provider = OpenAiRemoteOcr::new(client, policy, "provider.bailian.vision-ocr").unwrap();
        let mut options = ConversionOptions::default();
        options.network.enabled = true;
        options.network.deny_private_networks = false;
        options.network.allowed_hosts = vec!["127.0.0.1".into()];
        let context = context();
        let request = OcrRequest {
            image: b"\x89PNG\r\n\x1a\nfixture",
            media_type: "image/png",
            languages: &["zh-Hans"],
        };
        provider.planned_bound_output(request, &options, &context).unwrap();
        provider.planned_normalized_png_output(2480, 3508, &options, &context).unwrap();
        let recognition = block_on(provider.recognize_bound(request, &context)).unwrap();
        let OcrRecognition::Remote(result) = recognition else {
            panic!("remote OCR must use the typed remote result")
        };
        assert_eq!(result.provider, "provider.bailian.vision-ocr");
        assert_eq!(result.regions[0].text, "发票号码 12345");
        assert!(result.regions[0].confidence.abs() < f32::EPSILON);
        let request = worker.join().unwrap();
        assert!(request.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(request.windows(16).any(|window| window == b"data:image/png;b"));
    }

    #[test]
    fn remote_transcription_requires_audio_metadata_and_preserves_provenance() {
        let response = br#"{"id":"chatcmpl-asr","object":"chat.completion","created":1,"model":"qwen3-asr-flash","choices":[{"index":0,"message":{"role":"assistant","content":"\u6b22\u8fce\u4f7f\u7528\u3002","annotations":[{"type":"audio_info","language":"zh"}]},"finish_reason":"stop"}],"usage":{"seconds":3,"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        let (address, worker) = serve(response);
        let (client, _) =
            client(address, "audio-transcription", true, controlled_test_secret_environment());
        let provider = OpenAiRemoteTranscriber::new(
            client,
            "provider.bailian.audio-transcription",
            "qwen3-asr-flash",
        )
        .unwrap();
        let context = context();
        let media = wav_fixture(8_000, 8_000);
        let result = block_on(provider.transcribe(
            TranscriptionRequest { media: &media, media_type: "audio/wav", language: Some("zh") },
            &context,
        ))
        .unwrap();
        assert_eq!(result.provider, "provider.bailian.audio-transcription");
        assert_eq!(result.model, "qwen3-asr-flash");
        assert_eq!(result.language.as_deref(), Some("zh"));
        let Block::TimedSegment { range, content, .. } = &result.segments[0].block else {
            panic!("remote transcription must return a timed segment")
        };
        assert_eq!((range.start_ms, range.end_ms), (0, 3_000));
        assert_eq!(result.segments[0].provenance.provider, result.provider);
        assert_eq!(result.segments[0].provenance.locator.time, Some(*range));
        assert!(!content.is_empty());
        let request = worker.join().unwrap();
        assert!(request.windows(16).any(|window| window == b"data:audio/wav;b"));
        assert!(request.windows(12).any(|window| window == b"asr_options\""));
    }

    #[test]
    fn remote_transcription_sniffs_concrete_media_when_file_metadata_is_absent() {
        assert_eq!(
            canonical_remote_audio_media_type("audio/octet-stream", b"RIFF\0\0\0\0WAVEdata"),
            Some("audio/wav")
        );
        assert_eq!(
            canonical_remote_audio_media_type("application/octet-stream", b"ID3\x04\0\0"),
            Some("audio/mpeg")
        );
        assert_eq!(
            canonical_remote_audio_media_type("application/octet-stream", b"\0\0\0\x18ftypM4A "),
            Some("audio/mp4")
        );
        assert_eq!(
            canonical_remote_audio_media_type(
                "application/octet-stream",
                &[0x1a, 0x45, 0xdf, 0xa3, 0x93]
            ),
            Some("audio/webm")
        );
        assert_eq!(canonical_remote_audio_media_type("audio/x-wav", b"bad"), Some("audio/wav"));
        assert_eq!(canonical_remote_audio_media_type("application/octet-stream", b"bad"), None);
    }

    #[test]
    fn remote_duration_preflight_rejects_five_minutes_before_transport_and_allows_local_recovery() {
        let media = wav_fixture(301, 1);
        assert_eq!(wav_duration_ms(&media), Some(301_000));
        let provider = OpenAiRemoteTranscriber::new(
            client(
                "127.0.0.1:9".parse().unwrap(),
                "audio-transcription",
                false,
                "INTO_MD_DELIBERATELY_MISSING_REMOTE_TEST_KEY",
            )
            .0,
            "provider.bailian.audio-transcription",
            "qwen3-asr-flash",
        )
        .unwrap();
        let context = context();
        let error = block_on(provider.transcribe(
            TranscriptionRequest { media: &media, media_type: "audio/wav", language: None },
            &context,
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ResourceLimit { limit: "max_input_duration_ms", .. }
        ));
        assert!(provider.allows_prefer_fallback(&error));
        assert!(!provider.allows_prefer_fallback(&ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "fixture".into(),
        }));
    }

    #[test]
    fn network_denial_happens_before_secret_lookup() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (client, policy) =
            client(address, "vision-ocr", false, "INTO_MD_DELIBERATELY_MISSING_REMOTE_TEST_KEY");
        let provider = OpenAiRemoteOcr::new(client, policy, "provider.denied.vision-ocr").unwrap();
        let context = context();
        let error = block_on(provider.recognize(
            OcrRequest { image: b"png", media_type: "image/png", languages: &[] },
            &context,
        ))
        .unwrap_err();
        assert!(matches!(error, ConversionError::Network { .. }));
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
