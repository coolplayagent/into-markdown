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
            let result = self
                .client
                .generate(
                    GenerationRequest {
                        endpoint: GenerationEndpoint::ChatCompletions,
                        capability: "audio-transcription",
                        input: GenerationInput::Audio {
                            bytes: request.media,
                            media_type: request.media_type,
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
        let (client, policy) = client(address, "vision-ocr", true, "PATH");
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
        let (client, _) = client(address, "audio-transcription", true, "PATH");
        let provider = OpenAiRemoteTranscriber::new(
            client,
            "provider.bailian.audio-transcription",
            "qwen3-asr-flash",
        )
        .unwrap();
        let context = context();
        let result = block_on(provider.transcribe(
            TranscriptionRequest {
                media: b"RIFFfixture",
                media_type: "audio/wav",
                language: Some("zh"),
            },
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
