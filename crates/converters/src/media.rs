use into_markdown_core::{
    Block, BoxFuture, ChineseScript, ConversionError, ConversionOptions, Converter,
    ConverterOutput, Diagnostic, DiagnosticSeverity, DiarizationRequest, Document,
    ExecutionContext, FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
    TranscriptionRequest,
};

const MEDIA_FORMATS: &[InputFormat] = &[InputFormat::Audio, InputFormat::Video];
const PROVIDER_ID: &str = "builtin.converter.media-transcript";

/// Audio/video converter backed by an explicitly installed offline transcriber.
#[derive(Debug, Default)]
pub struct MediaConverter;

impl Converter for MediaConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        255
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        MEDIA_FORMATS
    }

    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if MEDIA_FORMATS.contains(&candidate.format) {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(context.available_memory_bytes())
    }

    // Transcription, optional diarization, and provenance validation form one ordered provider
    // transaction whose invariants are easier to audit together.
    #[allow(clippy::too_many_lines)]
    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if !MEDIA_FORMATS.contains(&candidate.format) {
                return Err(ConversionError::Unsupported {
                    detail: format!("media converter cannot handle {}", candidate.format.as_str()),
                });
            }
            let transcriber = services.transcriber.as_ref().ok_or_else(|| {
                ConversionError::ComponentUnavailable {
                    component: "whisper-small".into(),
                    detail: "media transcription requires an installed local media plugin or an enabled remote Provider".into(),
                }
            })?;
            let media_type =
                input.metadata.media_type.as_deref().unwrap_or(match candidate.format {
                    InputFormat::Audio => "audio/octet-stream",
                    InputFormat::Video => "video/octet-stream",
                    _ => unreachable!(),
                });
            let mut result = transcriber
                .transcribe(
                    TranscriptionRequest {
                        media: &input.bytes,
                        media_type,
                        language: options.asr.language.as_deref(),
                    },
                    context,
                )
                .await?;
            if !transcriber.accepts_result_provider(&result.provider)
                || result.model.is_empty()
                || result.segments.len()
                    > usize::try_from(options.asr.max_segments).unwrap_or(usize::MAX)
                || result
                    .language_confidence
                    .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
                || result
                    .segments
                    .iter()
                    .any(|node| !matches!(node.block, Block::TimedSegment { .. }))
            {
                return Err(ConversionError::Ai {
                    provider: transcriber.id().into(),
                    detail: "transcriber returned an invalid media-node contract".into(),
                });
            }
            let mut diarization_identity = None;
            if options.diarization.enabled {
                let diarizer = services.diarizer.as_ref().ok_or_else(|| {
                    ConversionError::ComponentUnavailable {
                        component: "speaker-diarization".into(),
                        detail: "speaker diarization requires the installed local media plugin"
                            .into(),
                    }
                })?;
                let diarization_result = diarizer
                    .diarize(
                        DiarizationRequest {
                            media: &input.bytes,
                            media_type,
                            segments: &result.segments,
                            expected_speakers: options.diarization.expected_speakers,
                            max_speakers: options.diarization.max_speakers,
                        },
                        context,
                    )
                    .await?;
                if diarization_result.provider != diarizer.id()
                    || diarization_result.model.is_empty()
                    || diarization_result.segments.len()
                        > usize::try_from(options.asr.max_segments).unwrap_or(usize::MAX)
                    || diarization_result.segments.iter().any(|node| match &node.block {
                        Block::TimedSegment { speaker, speaker_confidence, tokens, .. } => {
                            speaker.as_deref().is_some_and(|speaker| {
                                !valid_anonymous_speaker(speaker, options.diarization.max_speakers)
                            }) || speaker_confidence.is_some_and(|value| {
                                !value.is_finite() || !(0.0..=1.0).contains(&value)
                            }) || speaker.is_none() && speaker_confidence.is_some()
                                || tokens.iter().any(|token| {
                                    token.speaker.as_deref().is_some_and(|speaker| {
                                        !valid_anonymous_speaker(
                                            speaker,
                                            options.diarization.max_speakers,
                                        )
                                    }) || token.speaker.is_none()
                                        && token.speaker_confidence.is_some()
                                })
                        }
                        _ => true,
                    })
                {
                    return Err(ConversionError::Ai {
                        provider: diarizer.id().into(),
                        detail: "diarizer returned an invalid media-node contract".into(),
                    });
                }
                result.segments = diarization_result.segments;
                diarization_identity =
                    Some((diarization_result.provider, diarization_result.model));
            }
            let mut document = Document { blocks: result.segments, ..Document::default() };
            let diagnostics = if options.diarization.enabled {
                document
                    .blocks
                    .iter()
                    .filter_map(|node| match &node.block {
                        Block::TimedSegment { speaker: None, .. } => Some(Diagnostic {
                            code: "media.speakerAssignmentAmbiguous".into(),
                            severity: DiagnosticSeverity::Warning,
                            message: "No anonymous speaker was assigned because the local evidence was ambiguous.".into(),
                            locator: Some(node.provenance.locator.clone()),
                        }),
                        _ => None,
                    })
                    .take(1_024)
                    .collect()
            } else {
                Vec::new()
            };
            document.metadata.properties.insert("media.transcriber".into(), result.provider);
            document.metadata.properties.insert("media.model".into(), result.model);
            if let Some((provider, model)) = diarization_identity {
                document.metadata.properties.insert("media.diarizer".into(), provider);
                document.metadata.properties.insert("media.diarizationModel".into(), model);
            }
            if let Some(language) = result.language {
                document.metadata.properties.insert("media.language".into(), language);
            }
            if options.asr.chinese_script != ChineseScript::Preserve {
                document.metadata.properties.insert(
                    "media.chineseScript".into(),
                    match options.asr.chinese_script {
                        ChineseScript::Simplified => "simplified",
                        ChineseScript::Traditional => "traditional",
                        ChineseScript::Preserve => unreachable!(),
                    }
                    .into(),
                );
            }
            if let Some(confidence) = result.language_confidence {
                document
                    .metadata
                    .properties
                    .insert("media.languageConfidence".into(), format!("{confidence:.6}"));
            }
            Ok(ConverterOutput::new(document, Vec::new(), diagnostics))
        })
    }
}

fn valid_anonymous_speaker(value: &str, maximum: u16) -> bool {
    value.strip_prefix("speaker-").is_some_and(|number| {
        !number.is_empty()
            && !number.starts_with('0')
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && number.parse::<u16>().is_ok_and(|number| (1..=maximum).contains(&number))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        BlockNode, Inline, NodeId, Provenance, ProvenanceKind, SourceLocator, TimeRange,
        Transcriber, TranscriptionResult,
    };
    use std::sync::Arc;

    struct Stub;

    impl Transcriber for Stub {
        fn id(&self) -> &'static str {
            "test.transcriber"
        }

        fn transcribe<'a>(
            &'a self,
            _: TranscriptionRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
            Box::pin(async {
                let range = TimeRange { start_ms: 0, end_ms: 1_000 };
                Ok(TranscriptionResult {
                    segments: vec![BlockNode {
                        id: NodeId("segment-1".into()),
                        block: Block::TimedSegment {
                            range,
                            speaker: None,
                            speaker_confidence: None,
                            tokens: Vec::new(),
                            content: vec![Inline::Text {
                                value: "hello".into(),
                                marks: Vec::new(),
                            }],
                        },
                        provenance: Provenance {
                            kind: ProvenanceKind::AiProvider,
                            provider: "test.transcriber/model".into(),
                            locator: SourceLocator {
                                time: Some(range),
                                ..SourceLocator::default()
                            },
                            confidence: Some(0.9),
                        },
                    }],
                    provider: "test.transcriber".into(),
                    model: "model@sha256:abcd".into(),
                    language: Some("en".into()),
                    language_confidence: Some(0.95),
                })
            })
        }
    }

    #[test]
    fn transcription_becomes_timed_ir_with_language_and_model_metadata() {
        let input = ResolvedInput {
            bytes: Arc::from(&b"media"[..]),
            metadata: into_markdown_core::SourceMetadata {
                media_type: Some("audio/wav".into()),
                size: 5,
                ..into_markdown_core::SourceMetadata::default()
            },
        };
        let context = ExecutionContext::new(Default::default(), Default::default());
        let services = Services { transcriber: Some(Arc::new(Stub)), ..Services::default() };
        let output = futures::executor::block_on(MediaConverter.convert(
            &input,
            &FormatCandidate::explicit(InputFormat::Audio),
            &ConversionOptions::default(),
            &services,
            &context,
        ))
        .unwrap();
        assert_eq!(output.document.blocks.len(), 1);
        assert_eq!(output.document.metadata.properties["media.language"], "en");
        assert_eq!(output.document.metadata.properties["media.model"], "model@sha256:abcd");
    }

    #[test]
    fn anonymous_speaker_ids_are_canonical_and_bounded() {
        assert!(valid_anonymous_speaker("speaker-1", 16));
        assert!(valid_anonymous_speaker("speaker-16", 16));
        for invalid in ["speaker-0", "speaker-01", "speaker-17", "speaker-x", "Speaker 1"] {
            assert!(!valid_anonymous_speaker(invalid, 16));
        }
    }

    #[test]
    fn missing_service_is_a_stable_component_failure() {
        let input = ResolvedInput {
            bytes: Arc::from(&b"media"[..]),
            metadata: into_markdown_core::SourceMetadata {
                size: 5,
                ..into_markdown_core::SourceMetadata::default()
            },
        };
        let context = ExecutionContext::new(Default::default(), Default::default());
        let error = futures::executor::block_on(MediaConverter.convert(
            &input,
            &FormatCandidate::explicit(InputFormat::Audio),
            &ConversionOptions::default(),
            &Services::default(),
            &context,
        ))
        .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ComponentUnavailable);
    }
}
