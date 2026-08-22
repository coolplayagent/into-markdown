//! Local anonymous speaker diarization over Silero VAD and 3D-Speaker embeddings.

use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, DiarizationRequest, DiarizationResult, Diarizer,
    ExecutionContext, ExecutionStage, Inline, MEDIA_CHECKPOINT_SCHEMA_VERSION, MediaCheckpoint,
    MediaCheckpointStage, MediaSpeakerCluster, NodeId, NormalizedAudioIdentity,
    RecoveredMediaCheckpoint, ResourceReservation, Tensor, TensorRuntime, TimeRange, TimedToken,
    estimate_retained_blocks,
};
use into_markdown_ffmpeg::{FfmpegRuntime, MediaLimits, NormalizedAudio};
use into_markdown_ocr::{
    Dimension, ModelContract, ModelIdentity, ModelManager, ModelManagerError, ModelResolver,
    ResolvedModel, TensorElementType, TensorSpec,
};
use std::collections::{BTreeMap, BTreeSet};
use std::f32::consts::PI;
use std::sync::{Arc, Mutex};

const PROVIDER_ID: &str = "builtin.diarization.silero-3dspeaker";
const BUNDLE_ID: &str = "silero-vad-3dspeaker-eres2net";
const SAMPLE_RATE: u32 = 16_000;
const VAD_MODEL_ID: &str = "silero-vad";
const EMBEDDING_MODEL_ID: &str = "3dspeaker-embedding";
const VAD_WINDOW: usize = 512;
const EMBEDDING_DIMENSION: usize = 512;
const MIN_EMBEDDING_SAMPLES: usize = 16_000;
const MAX_EMBEDDING_SAMPLES: usize = 160_000;
const AUTO_CREATE_THRESHOLD: f32 = 0.20;
const EXPECTED_CREATE_THRESHOLD: f32 = 0.45;
const ASSIGN_THRESHOLD: f32 = 0.58;
const AMBIGUITY_MARGIN: f32 = 0.035;
const TURN_WINDOW_MS: u64 = 3_000;
const SAME_SPEAKER_MERGE_GAP_MS: u64 = 1_500;
const CHECKPOINT_INTERVAL_MS: u64 = 30_000;

/// Product resolver for the two hash-verified diarization ONNX models.
pub struct DiarizationModelResolver {
    cache: Mutex<BTreeMap<String, CachedArtifact>>,
}

#[derive(Clone)]
struct CachedArtifact {
    identity: ModelIdentity,
    bytes: Arc<[u8]>,
    // The bounded two-entry process cache owns the original charge for these
    // immutable bytes. Each resolve also reserves the model size against the
    // current request before native inference begins.
    _memory_reservation: Arc<ResourceReservation>,
}

impl DiarizationModelResolver {
    /// Create a resolver backed by the installed model manager and preload the
    /// bounded two-model cache under the service-assembly budget. Request
    /// preflight credits must never escape through a process-local cache.
    ///
    /// # Errors
    ///
    /// Returns a stable model or resource error when either reviewed artifact
    /// cannot be verified and retained.
    pub fn new(
        manager: Arc<ModelManager>,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let mut cache = BTreeMap::new();
        for (model_id, role) in [(VAD_MODEL_ID, "vad"), (EMBEDDING_MODEL_ID, "speaker-embedding")] {
            let artifact = manager
                .verified_runtime_artifact(BUNDLE_ID, role, context)
                .map_err(map_model_error)?;
            let bytes = u64::try_from(artifact.bytes.len()).map_err(|_| {
                ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "diarization model length overflowed".into(),
                }
            })?;
            cache.insert(
                model_id.to_owned(),
                CachedArtifact {
                    identity: ModelIdentity {
                        canonical_path: artifact.path,
                        sha256: artifact.sha256,
                        bytes,
                        file_identity: artifact.file_identity,
                    },
                    bytes: artifact.bytes,
                    _memory_reservation: artifact.memory_reservation,
                },
            );
        }
        Ok(Self { cache: Mutex::new(cache) })
    }
}

impl ModelResolver for DiarizationModelResolver {
    fn resolve(
        &self,
        model_id: &str,
        context: &ExecutionContext,
    ) -> Result<ResolvedModel, ConversionError> {
        context.checkpoint()?;
        let (role, contract) = match model_id {
            VAD_MODEL_ID => ("vad", silero_contract()),
            EMBEDDING_MODEL_ID => ("speaker-embedding", embedding_contract()),
            _ => {
                return Err(ConversionError::ComponentUnavailable {
                    component: model_id.into(),
                    detail: "unknown diarization model".into(),
                });
            }
        };
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(model_id)
            .cloned()
            .ok_or_else(|| ConversionError::ComponentUnavailable {
                component: role.into(),
                detail: "diarization model cache was not assembled".into(),
            })?;
        let request_memory = context.reserve_memory(cached.identity.bytes)?;
        Ok(ResolvedModel {
            identity: cached.identity,
            contract,
            bytes: cached.bytes,
            memory_reservation: Some(Arc::new(request_memory)),
        })
    }
}

fn map_model_error(error: ModelManagerError) -> ConversionError {
    match error {
        ModelManagerError::Execution(error) => error,
        other => ConversionError::ComponentUnavailable {
            component: BUNDLE_ID.into(),
            detail: format!(
                "installed Speech capability verification failed ({other}); repair or reinstall the Speech plugin"
            ),
        },
    }
}

fn spec(name: &str, dimensions: Vec<Dimension>) -> TensorSpec {
    TensorSpec { name: name.into(), element_type: TensorElementType::Float32, dimensions }
}

fn silero_contract() -> ModelContract {
    ModelContract {
        ir_version: 8,
        opsets: BTreeMap::from([(String::new(), 16)]),
        inputs: vec![
            spec(
                "input",
                vec![
                    Dimension::Dynamic { min: 1, max: 8 },
                    Dimension::Dynamic { min: 1, max: 4096 },
                ],
            ),
            spec(
                "state",
                vec![
                    Dimension::Exact(2),
                    Dimension::Dynamic { min: 1, max: 8 },
                    Dimension::Exact(128),
                ],
            ),
        ],
        overridable_inputs: Vec::new(),
        outputs: vec![
            spec("output", vec![Dimension::Dynamic { min: 1, max: 8 }, Dimension::Exact(1)]),
            spec(
                "stateN",
                vec![
                    Dimension::Dynamic { min: 2, max: 2 },
                    Dimension::Dynamic { min: 1, max: 8 },
                    Dimension::Dynamic { min: 128, max: 128 },
                ],
            ),
        ],
        session_memory_bytes: 32 * 1024 * 1024,
        run_memory_bytes: 8 * 1024 * 1024,
    }
}

fn embedding_contract() -> ModelContract {
    ModelContract {
        ir_version: 7,
        opsets: BTreeMap::from([(String::new(), 13)]),
        inputs: vec![spec(
            "x",
            vec![
                Dimension::Dynamic { min: 1, max: 8 },
                Dimension::Dynamic { min: 1, max: 2_000 },
                Dimension::Exact(80),
            ],
        )],
        overridable_inputs: Vec::new(),
        outputs: vec![spec(
            "embedding",
            vec![Dimension::Dynamic { min: 1, max: 8 }, Dimension::Exact(512)],
        )],
        session_memory_bytes: 192 * 1024 * 1024,
        run_memory_bytes: 64 * 1024 * 1024,
    }
}

#[derive(Debug, Clone)]
struct Cluster {
    centroid: Vec<f32>,
    observations: u16,
}

/// Bounded online cosine clustering with stable first-appearance IDs.
#[derive(Debug, Clone)]
pub struct OnlineCosineClustering {
    clusters: Vec<Cluster>,
    expected_speakers: Option<u16>,
    max_speakers: u16,
}

impl OnlineCosineClustering {
    /// Create a bounded clustering state.
    ///
    /// # Errors
    ///
    /// Returns a stable configuration error when the speaker bounds are empty,
    /// inverted, or exceed the supported 64-cluster limit.
    pub fn new(expected_speakers: Option<u16>, max_speakers: u16) -> Result<Self, ConversionError> {
        if max_speakers == 0
            || max_speakers > 64
            || expected_speakers.is_some_and(|value| value == 0 || value > max_speakers)
        {
            return Err(ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "diarization speaker bounds are invalid".into(),
            });
        }
        Ok(Self { clusters: Vec::new(), expected_speakers, max_speakers })
    }

    /// Assign one normalized embedding to the closest bounded cluster.
    ///
    /// Low-separation assignments keep a reduced confidence and do not update a
    /// centroid. A transcript token is known speech, so dropping its speaker
    /// entirely produces alternating labelled and anonymous fragments without
    /// improving the underlying clustering decision.
    pub fn assign(&mut self, embedding: &[f32]) -> Option<(String, f32)> {
        if embedding.len() != EMBEDDING_DIMENSION
            || embedding.iter().any(|value| !value.is_finite())
            || embedding.iter().map(|value| value * value).sum::<f32>() <= f32::EPSILON
        {
            return None;
        }
        if self.clusters.is_empty() {
            return self.create(embedding);
        }
        let mut ranked = self
            .clusters
            .iter()
            .enumerate()
            .map(|(index, cluster)| (index, cosine(&cluster.centroid, embedding)))
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
        let (best_index, best) = ranked[0];
        let runner_up = ranked.get(1).map_or(-1.0, |value| value.1);
        let target = usize::from(self.expected_speakers.unwrap_or(self.max_speakers));
        let create_threshold = if self.expected_speakers.is_some() {
            EXPECTED_CREATE_THRESHOLD
        } else {
            AUTO_CREATE_THRESHOLD
        };
        let should_create = self.clusters.len() < target && best < create_threshold;
        if should_create {
            return self.create(embedding);
        }
        let separation = best - runner_up;
        let reliable = best >= ASSIGN_THRESHOLD && separation >= AMBIGUITY_MARGIN;
        let cluster = &mut self.clusters[best_index];
        cluster.observations = cluster.observations.saturating_add(1);
        if reliable {
            let weight = 1.0 / f32::from(cluster.observations);
            for (centroid, value) in cluster.centroid.iter_mut().zip(embedding) {
                *centroid += (*value - *centroid) * weight;
            }
            normalize(&mut cluster.centroid);
        }
        let separation_confidence =
            (separation / AMBIGUITY_MARGIN).clamp(0.0, 1.0).mul_add(0.5, 0.5);
        Some((format!("speaker-{}", best_index + 1), confidence(best) * separation_confidence))
    }

    fn automatic_singleton_remap(&self) -> BTreeMap<String, String> {
        if self.expected_speakers.is_some() {
            return BTreeMap::new();
        }
        let stable = self
            .clusters
            .iter()
            .enumerate()
            .filter(|(_, cluster)| cluster.observations >= 2)
            .collect::<Vec<_>>();
        if stable.is_empty() {
            return BTreeMap::new();
        }
        let singletons = self
            .clusters
            .iter()
            .enumerate()
            .filter(|(_, cluster)| cluster.observations == 1)
            .collect::<Vec<_>>();
        if singletons.len() <= 1 {
            return BTreeMap::new();
        }
        let keep = singletons
            .iter()
            .min_by(|left, right| {
                closest_similarity(left.1, &stable).total_cmp(&closest_similarity(right.1, &stable))
            })
            .map(|(index, _)| *index);
        singletons
            .into_iter()
            .filter(|(index, _)| Some(*index) != keep)
            .filter_map(|(index, cluster)| {
                stable
                    .iter()
                    .max_by(|left, right| {
                        cosine(&left.1.centroid, &cluster.centroid)
                            .total_cmp(&cosine(&right.1.centroid, &cluster.centroid))
                    })
                    .map(|(target, _)| {
                        (format!("speaker-{}", index + 1), format!("speaker-{}", target + 1))
                    })
            })
            .collect()
    }

    /// Restore a bounded clustering state after its durable identity was verified.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured bounds or a restored centroid are invalid.
    pub fn from_checkpoint(
        expected_speakers: Option<u16>,
        max_speakers: u16,
        clusters: Vec<MediaSpeakerCluster>,
    ) -> Result<Self, ConversionError> {
        let mut state = Self::new(expected_speakers, max_speakers)?;
        if clusters.len() > usize::from(max_speakers) {
            return Err(recovery("speaker checkpoint exceeds the configured cluster bound"));
        }
        for cluster in clusters {
            if cluster.observations == 0
                || cluster.centroid.len() != EMBEDDING_DIMENSION
                || cluster.centroid.iter().any(|value| !value.is_finite())
            {
                return Err(recovery("speaker checkpoint contains an invalid centroid"));
            }
            let mut centroid = cluster.centroid;
            normalize(&mut centroid);
            state.clusters.push(Cluster { centroid, observations: cluster.observations });
        }
        Ok(state)
    }

    /// Snapshot the fixed-memory clustering state for a durable media checkpoint.
    #[must_use]
    pub fn checkpoint(&self) -> Vec<MediaSpeakerCluster> {
        self.clusters
            .iter()
            .map(|cluster| MediaSpeakerCluster {
                centroid: cluster.centroid.clone(),
                observations: cluster.observations,
            })
            .collect()
    }

    fn create(&mut self, embedding: &[f32]) -> Option<(String, f32)> {
        if self.clusters.len() >= usize::from(self.max_speakers) {
            return None;
        }
        let mut centroid = embedding.to_vec();
        normalize(&mut centroid);
        self.clusters.push(Cluster { centroid, observations: 1 });
        Some((format!("speaker-{}", self.clusters.len()), 1.0))
    }
}

fn closest_similarity(cluster: &Cluster, candidates: &[(usize, &Cluster)]) -> f32 {
    candidates
        .iter()
        .map(|(_, candidate)| cosine(&candidate.centroid, &cluster.centroid))
        .max_by(f32::total_cmp)
        .unwrap_or(-1.0)
}

fn recovery(detail: impl Into<String>) -> ConversionError {
    ConversionError::Recovery { reason: "corrupt", detail: detail.into() }
}

fn confidence(cosine_similarity: f32) -> f32 {
    ((cosine_similarity + 1.0) * 0.5).clamp(0.0, 1.0)
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(left, right)| left * right).sum::<f32>().clamp(-1.0, 1.0)
}

fn normalize(values: &mut [f32]) {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in values {
            *value /= norm;
        }
    }
}

/// Production local diarizer using the isolated tensor runtime.
pub struct LocalSpeakerDiarizer {
    runtime: Arc<dyn TensorRuntime>,
    ffmpeg: Arc<FfmpegRuntime>,
    model_identity: String,
}

impl LocalSpeakerDiarizer {
    /// Construct from verified runtime services.
    #[must_use]
    pub fn new(
        runtime: Arc<dyn TensorRuntime>,
        ffmpeg: Arc<FfmpegRuntime>,
        model_identity: String,
    ) -> Self {
        Self { runtime, ffmpeg, model_identity }
    }

    async fn diarize_inner(
        &self,
        request: DiarizationRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<DiarizationResult, ConversionError> {
        context.checkpoint()?;
        context.report(
            ExecutionStage::Ai,
            Some(660),
            Some(1_000),
            Some("diarization.normalize"),
        )?;
        let audio = self.ffmpeg.normalize_to_file_with_progress(
            request.media,
            MediaLimits {
                max_input_bytes: u64::try_from(request.media.len()).unwrap_or(u64::MAX),
                max_duration_ms: None,
                sample_rate: SAMPLE_RATE,
                channels: 1,
                ..MediaLimits::default()
            },
            context,
            "diarization.normalize",
        )?;
        let audio_identity = NormalizedAudioIdentity {
            sha256: audio.sha256.clone(),
            frames: audio.frames,
            sample_rate: audio.sample_rate,
            channels: audio.channels,
        };
        let recovered = context.load_media_checkpoint()?.map(RecoveredMediaCheckpoint::into_state);
        let (
            mut segments,
            mut clustering,
            completed_segments,
            transcriber_provider,
            transcriber_model,
            language,
            language_confidence,
        ) = if let Some(checkpoint) = recovered {
            if checkpoint.audio != audio_identity {
                return Err(ConversionError::Recovery {
                    reason: "incompatible",
                    detail: "normalized audio changed after the diarization checkpoint".into(),
                });
            }
            if checkpoint.stage == MediaCheckpointStage::Transcribing
                && checkpoint.next_window_start_frame != audio.frames
            {
                return Err(recovery(
                    "diarization cannot resume before transcription reaches the final frame",
                ));
            }
            if checkpoint.segments != request.segments {
                return Err(recovery(
                    "diarization request does not match the checkpoint transcript",
                ));
            }
            if checkpoint.stage == MediaCheckpointStage::Diarizing
                && (checkpoint.diarizer_provider.as_deref() != Some(PROVIDER_ID)
                    || checkpoint.diarization_model.as_deref()
                        != Some(self.model_identity.as_str()))
            {
                return Err(ConversionError::Recovery {
                    reason: "incompatible",
                    detail: "diarization model changed after the media checkpoint".into(),
                });
            }
            let clustering = OnlineCosineClustering::from_checkpoint(
                request.expected_speakers,
                request.max_speakers,
                checkpoint.speaker_clusters,
            )?;
            (
                checkpoint.segments,
                clustering,
                usize::try_from(checkpoint.diarization_completed_segments)
                    .map_err(|_| recovery("diarization checkpoint index overflowed"))?,
                checkpoint.transcriber_provider,
                checkpoint.transcriber_model,
                checkpoint.language,
                checkpoint.language_confidence,
            )
        } else {
            let provider = request
                .segments
                .first()
                .map_or("builtin.asr.unknown", |node| node.provenance.provider.as_str());
            (
                request.segments.to_vec(),
                OnlineCosineClustering::new(request.expected_speakers, request.max_speakers)?,
                0,
                provider.split('/').next().unwrap_or(provider).to_owned(),
                provider.split_once('/').map_or("unknown", |(_, model)| model).to_owned(),
                None,
                None,
            )
        };
        if completed_segments > segments.len() {
            return Err(recovery("diarization checkpoint index exceeds the transcript"));
        }
        let total_segments = u64::try_from(segments.len()).unwrap_or(u64::MAX).max(1);
        let mut next_checkpoint_ms = completed_segments
            .checked_sub(1)
            .and_then(|index| segments.get(index))
            .and_then(|node| match node.block {
                Block::TimedSegment { range, .. } => Some(range.end_ms),
                _ => None,
            })
            .unwrap_or(0)
            .saturating_add(CHECKPOINT_INTERVAL_MS);
        for index in completed_segments..segments.len() {
            context.checkpoint()?;
            let completed = u64::try_from(index).unwrap_or(u64::MAX).min(total_segments);
            context.report(
                ExecutionStage::Ai,
                Some(680 + completed.saturating_mul(270) / total_segments),
                Some(1_000),
                Some("diarization.inference"),
            )?;
            let (segment_range, turns) = match &segments[index].block {
                Block::TimedSegment { range, tokens, .. } => (*range, token_turns(tokens, *range)),
                _ => {
                    return Err(ConversionError::Ai {
                        provider: PROVIDER_ID.into(),
                        detail: "diarization input contains a non-timed node".into(),
                    });
                }
            };
            if turns.is_empty() {
                let assignment = self
                    .assign_range(&audio, segment_range, false, &mut clustering, context)
                    .await?;
                let Block::TimedSegment { speaker, speaker_confidence, .. } =
                    &mut segments[index].block
                else {
                    return Err(ConversionError::Ai {
                        provider: PROVIDER_ID.into(),
                        detail: "diarization input contains a non-timed node".into(),
                    });
                };
                if let Some((id, confidence)) = assignment {
                    *speaker = Some(id);
                    *speaker_confidence = Some(confidence);
                } else {
                    *speaker = None;
                    *speaker_confidence = None;
                }
            } else {
                for turn in turns {
                    let assignment = self
                        .assign_range(&audio, turn.range, true, &mut clustering, context)
                        .await?;
                    let Block::TimedSegment { tokens, .. } = &mut segments[index].block else {
                        return Err(ConversionError::Ai {
                            provider: PROVIDER_ID.into(),
                            detail: "diarization input contains a non-timed node".into(),
                        });
                    };
                    for token in &mut tokens[turn.start..turn.end] {
                        if let Some((id, confidence)) = &assignment {
                            token.speaker = Some(id.clone());
                            token.speaker_confidence = Some(*confidence);
                        } else {
                            token.speaker = None;
                            token.speaker_confidence = None;
                        }
                    }
                }
                summarize_token_speaker(&mut segments[index])?;
            }
            if segment_range.end_ms >= next_checkpoint_ms || index + 1 == segments.len() {
                let cluster_clone_bytes = u64::try_from(clustering.clusters.len())
                    .ok()
                    .and_then(|count| {
                        count.checked_mul(
                            u64::try_from(EMBEDDING_DIMENSION * std::mem::size_of::<f32>() + 64)
                                .ok()?,
                        )
                    })
                    .ok_or_else(|| ConversionError::ResourceLimit {
                        limit: "max_memory_bytes",
                        detail: "speaker checkpoint memory estimate overflowed".into(),
                    })?;
                let checkpoint_clone_bytes = estimate_retained_blocks(&segments)?
                    .checked_add(cluster_clone_bytes)
                    .and_then(|bytes| bytes.checked_add(1_024))
                    .ok_or_else(|| ConversionError::ResourceLimit {
                        limit: "max_memory_bytes",
                        detail: "speaker checkpoint memory estimate overflowed".into(),
                    })?;
                let _checkpoint_memory = context.reserve_memory(checkpoint_clone_bytes)?;
                context.commit_media_checkpoint(&MediaCheckpoint {
                    schema_version: MEDIA_CHECKPOINT_SCHEMA_VERSION,
                    audio: audio_identity.clone(),
                    stage: MediaCheckpointStage::Diarizing,
                    next_window_start_frame: audio.frames,
                    segments: segments.clone(),
                    transcriber_provider: transcriber_provider.clone(),
                    transcriber_model: transcriber_model.clone(),
                    language: language.clone(),
                    language_confidence,
                    diarizer_provider: Some(PROVIDER_ID.into()),
                    diarization_model: Some(self.model_identity.clone()),
                    diarization_completed_segments: u32::try_from(index + 1)
                        .map_err(|_| recovery("diarization checkpoint index overflowed"))?,
                    speaker_clusters: clustering.checkpoint(),
                })?;
                next_checkpoint_ms = segment_range.end_ms.saturating_add(CHECKPOINT_INTERVAL_MS);
            }
        }
        remap_speakers(&mut segments, &clustering.automatic_singleton_remap())?;
        if request.expected_speakers.is_none() {
            compact_speaker_labels(&mut segments)?;
        }
        repair_unassigned_speech(&mut segments)?;
        let segments = merge_adjacent_speaker_turns(split_token_speaker_turns(segments)?)?;
        context.report(ExecutionStage::Ai, Some(950), Some(1_000), Some("diarization.complete"))?;
        Ok(DiarizationResult {
            segments,
            provider: PROVIDER_ID.into(),
            model: self.model_identity.clone(),
        })
    }

    async fn assign_range(
        &self,
        audio: &NormalizedAudio,
        range: TimeRange,
        known_speech: bool,
        clustering: &mut OnlineCosineClustering,
        context: &ExecutionContext,
    ) -> Result<Option<(String, f32)>, ConversionError> {
        let start = milliseconds_to_frame(range.start_ms, audio.frames);
        let end = milliseconds_to_frame(range.end_ms, audio.frames).max(start);
        let samples = read_segment_samples(audio, start, end, context)?;
        let voice =
            if known_speech { 1.0 } else { self.voice_probability(&samples, context).await? };
        if voice < 0.45 || samples.len() < MIN_EMBEDDING_SAMPLES {
            return Ok(None);
        }
        let features = log_mel_fbank(&samples)?;
        let frames = features.len() / 80;
        let output = self
            .runtime
            .run(
                EMBEDDING_MODEL_ID,
                &[Tensor { shape: vec![1, frames, 80], values: features }],
                context,
            )
            .await?;
        let mut embedding = output
            .into_iter()
            .next()
            .filter(|tensor| tensor.shape == [1, EMBEDDING_DIMENSION])
            .map(|tensor| tensor.values)
            .ok_or_else(|| ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "speaker embedding output contract changed".into(),
            })?;
        normalize(&mut embedding);
        Ok(clustering
            .assign(&embedding)
            .map(|(id, confidence)| (id, (confidence * voice).clamp(0.0, 1.0))))
    }

    async fn voice_probability(
        &self,
        samples: &[f32],
        context: &ExecutionContext,
    ) -> Result<f32, ConversionError> {
        let mut state = vec![0.0; 2 * 128];
        let mut probabilities = Vec::new();
        probabilities.try_reserve(samples.len().div_ceil(VAD_WINDOW)).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "speaker VAD probability allocation failed".into(),
            }
        })?;
        for chunk in samples.chunks(VAD_WINDOW) {
            context.checkpoint()?;
            let mut input = vec![0.0; VAD_WINDOW];
            input[..chunk.len()].copy_from_slice(chunk);
            let outputs = self
                .runtime
                .run(
                    VAD_MODEL_ID,
                    &[
                        Tensor { shape: vec![1, VAD_WINDOW], values: input },
                        Tensor { shape: vec![2, 1, 128], values: state },
                    ],
                    context,
                )
                .await?;
            if outputs.len() != 2
                || outputs[0].shape != [1, 1]
                || outputs[0].values.len() != 1
                || outputs[1].shape != [2, 1, 128]
                || outputs[1].values.len() != 256
            {
                return Err(ConversionError::Ai {
                    provider: PROVIDER_ID.into(),
                    detail: "Silero VAD output contract changed".into(),
                });
            }
            let probability = outputs[0].values[0];
            if !probability.is_finite() || outputs[1].values.iter().any(|value| !value.is_finite())
            {
                return Err(ConversionError::Ai {
                    provider: PROVIDER_ID.into(),
                    detail: "Silero VAD returned non-finite values".into(),
                });
            }
            probabilities.push(probability.clamp(0.0, 1.0));
            state = outputs.into_iter().nth(1).expect("checked output count").values;
        }
        Ok(robust_voice_probability(&mut probabilities))
    }
}

#[derive(Clone, Copy)]
struct TokenTurn {
    start: usize,
    end: usize,
    range: TimeRange,
}

fn token_turns(tokens: &[TimedToken], fallback: TimeRange) -> Vec<TokenTurn> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let first_start = tokens[0].range.start_ms.max(fallback.start_ms);
    let final_end = tokens[tokens.len() - 1].range.end_ms.min(fallback.end_ms);
    let duration = final_end.saturating_sub(first_start);
    let group_count = usize::try_from(duration.div_ceil(TURN_WINDOW_MS))
        .unwrap_or(tokens.len())
        .clamp(1, tokens.len());
    let mut turns = Vec::with_capacity(group_count);
    let mut start = 0_usize;
    for boundary in 1..=group_count {
        let end = if boundary == group_count {
            tokens.len()
        } else {
            let ideal = first_start.saturating_add(
                duration.saturating_mul(u64::try_from(boundary).unwrap_or(u64::MAX))
                    / u64::try_from(group_count).unwrap_or(1),
            );
            let remaining_groups = group_count - boundary;
            let latest = tokens.len() - remaining_groups;
            (start + 1..=latest)
                .min_by_key(|candidate| tokens[candidate - 1].range.end_ms.abs_diff(ideal))
                .unwrap_or(start + 1)
        };
        if end <= start {
            continue;
        }
        let range = TimeRange {
            start_ms: tokens[start].range.start_ms.max(fallback.start_ms),
            end_ms: tokens[end - 1].range.end_ms.min(fallback.end_ms),
        };
        if range.start_ms < range.end_ms {
            turns.push(TokenTurn { start, end, range });
        }
        start = end;
    }
    turns
}

fn robust_voice_probability(probabilities: &mut [f32]) -> f32 {
    if probabilities.is_empty() {
        return 0.0;
    }
    probabilities.sort_unstable_by(|left, right| right.total_cmp(left));
    let speech_frames = probabilities.len().div_ceil(4).max(1);
    let total = probabilities[..speech_frames].iter().sum::<f32>();
    let count = u16::try_from(speech_frames).unwrap_or(u16::MAX);
    total / f32::from(count)
}

fn repair_unassigned_speech(segments: &mut [BlockNode]) -> Result<(), ConversionError> {
    let mut speakers = BTreeSet::new();
    for node in segments.iter() {
        let Block::TimedSegment { speaker, tokens, .. } = &node.block else {
            return Err(ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "diarization input contains a non-timed node".into(),
            });
        };
        if let Some(speaker) = speaker {
            speakers.insert(speaker.clone());
        }
        speakers.extend(tokens.iter().filter_map(|token| token.speaker.clone()));
    }
    let only_speaker = (speakers.len() == 1).then(|| speakers.into_iter().next()).flatten();
    for node in segments {
        let has_tokens = {
            let Block::TimedSegment { speaker, speaker_confidence, tokens, .. } = &mut node.block
            else {
                return Err(ConversionError::Ai {
                    provider: PROVIDER_ID.into(),
                    detail: "diarization input contains a non-timed node".into(),
                });
            };
            if let Some(id) = &only_speaker {
                for token in tokens.iter_mut().filter(|token| token.speaker.is_none()) {
                    token.speaker = Some(id.clone());
                    token.speaker_confidence = Some(0.0);
                }
                if tokens.is_empty() && speaker.is_none() {
                    *speaker = Some(id.clone());
                    *speaker_confidence = Some(0.0);
                }
            } else {
                repair_bracketed_token_gaps(tokens);
            }
            !tokens.is_empty()
        };
        if has_tokens {
            summarize_token_speaker(node)?;
        }
    }
    Ok(())
}

fn remap_speakers(
    segments: &mut [BlockNode],
    remap: &BTreeMap<String, String>,
) -> Result<(), ConversionError> {
    if remap.is_empty() {
        return Ok(());
    }
    for node in segments {
        let Block::TimedSegment { speaker, tokens, .. } = &mut node.block else {
            return Err(ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "diarization input contains a non-timed node".into(),
            });
        };
        if let Some(target) = speaker.as_ref().and_then(|id| remap.get(id)) {
            *speaker = Some(target.clone());
        }
        for token in tokens {
            if let Some(target) = token.speaker.as_ref().and_then(|id| remap.get(id)) {
                token.speaker = Some(target.clone());
            }
        }
    }
    Ok(())
}

fn compact_speaker_labels(segments: &mut [BlockNode]) -> Result<(), ConversionError> {
    let mut remap = BTreeMap::new();
    for node in segments.iter() {
        let Block::TimedSegment { speaker, tokens, .. } = &node.block else {
            return Err(ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "diarization input contains a non-timed node".into(),
            });
        };
        for id in tokens.iter().filter_map(|token| token.speaker.as_ref()).chain(speaker) {
            if !remap.contains_key(id) {
                remap.insert(id.clone(), format!("speaker-{}", remap.len() + 1));
            }
        }
    }
    remap_speakers(segments, &remap)
}

fn repair_bracketed_token_gaps(tokens: &mut [TimedToken]) {
    let mut start = 0_usize;
    while start < tokens.len() {
        if tokens[start].speaker.is_some() {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < tokens.len() && tokens[end].speaker.is_none() {
            end += 1;
        }
        let before = start.checked_sub(1).and_then(|index| tokens[index].speaker.clone());
        let after = tokens.get(end).and_then(|token| token.speaker.clone());
        if let Some(id) = before.filter(|id| after.as_ref() == Some(id)) {
            let confidence = start
                .checked_sub(1)
                .and_then(|index| tokens[index].speaker_confidence)
                .zip(tokens.get(end).and_then(|token| token.speaker_confidence))
                .map_or(0.0, |(left, right)| left.min(right) * 0.5);
            for token in &mut tokens[start..end] {
                token.speaker = Some(id.clone());
                token.speaker_confidence = Some(confidence);
            }
        }
        start = end;
    }
}

fn summarize_token_speaker(node: &mut BlockNode) -> Result<(), ConversionError> {
    let Block::TimedSegment { speaker, speaker_confidence, tokens, .. } = &mut node.block else {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "diarization input contains a non-timed node".into(),
        });
    };
    let first = tokens.first().and_then(|token| token.speaker.clone());
    if first.is_some() && tokens.iter().all(|token| token.speaker == first) {
        let (total, count) = tokens.iter().fold((0.0_f64, 0_u32), |(total, count), token| {
            token.speaker_confidence.map_or((total, count), |confidence| {
                (total + f64::from(confidence), count.saturating_add(1))
            })
        });
        *speaker = first;
        *speaker_confidence = (count != 0).then(|| (total / f64::from(count)) as f32);
    } else {
        *speaker = None;
        *speaker_confidence = None;
    }
    Ok(())
}

struct TokenSpeakerGroup {
    speaker: Option<String>,
    confidence_total: f64,
    confidence_count: u32,
    tokens: Vec<TimedToken>,
    text: String,
}

impl TokenSpeakerGroup {
    fn new(token: TimedToken) -> Self {
        let speaker = token.speaker.clone();
        let (confidence_total, confidence_count) =
            token.speaker_confidence.map_or((0.0, 0), |value| (f64::from(value), 1));
        let text = token.text.clone();
        Self { speaker, confidence_total, confidence_count, tokens: vec![token], text }
    }

    fn push(&mut self, token: TimedToken) -> Result<(), ConversionError> {
        self.text.try_reserve(token.text.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "speaker turn transcript allocation failed".into(),
        })?;
        self.text.push_str(&token.text);
        if let Some(confidence) = token.speaker_confidence {
            self.confidence_total += f64::from(confidence);
            self.confidence_count = self.confidence_count.saturating_add(1);
        }
        self.tokens.try_reserve(1).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "speaker turn token allocation failed".into(),
        })?;
        self.tokens.push(token);
        Ok(())
    }
}

fn split_token_speaker_turns(segments: Vec<BlockNode>) -> Result<Vec<BlockNode>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve(segments.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "speaker turn output allocation failed".into(),
    })?;
    for node in segments {
        let BlockNode { id, block, provenance } = node;
        let Block::TimedSegment { range, speaker, speaker_confidence, tokens, content } = block
        else {
            return Err(ConversionError::Ai {
                provider: PROVIDER_ID.into(),
                detail: "diarization output contains a non-timed node".into(),
            });
        };
        if tokens.is_empty() {
            output.push(BlockNode {
                id,
                block: Block::TimedSegment { range, speaker, speaker_confidence, tokens, content },
                provenance,
            });
            continue;
        }
        let mut groups = Vec::<TokenSpeakerGroup>::new();
        for token in tokens {
            if groups.last().is_some_and(|group| group.speaker == token.speaker) {
                groups.last_mut().expect("checked last group").push(token)?;
            } else {
                groups.try_reserve(1).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "speaker turn group allocation failed".into(),
                })?;
                groups.push(TokenSpeakerGroup::new(token));
            }
        }
        let split = groups.len() > 1;
        for (index, group) in groups.into_iter().enumerate() {
            let start_ms =
                group.tokens.first().map_or(range.start_ms, |token| token.range.start_ms);
            let end_ms = group.tokens.last().map_or(range.end_ms, |token| token.range.end_ms);
            let turn_range = TimeRange { start_ms, end_ms };
            let confidence = (group.confidence_count != 0)
                .then(|| (group.confidence_total / f64::from(group.confidence_count)) as f32);
            let mut turn_provenance = provenance.clone();
            turn_provenance.locator.time = Some(turn_range);
            output.push(BlockNode {
                id: if split {
                    NodeId(format!("{}-turn-{:04}", id.0, index + 1))
                } else {
                    id.clone()
                },
                block: Block::TimedSegment {
                    range: turn_range,
                    speaker: group.speaker,
                    speaker_confidence: confidence,
                    tokens: group.tokens,
                    content: vec![Inline::Text { value: group.text, marks: Vec::new() }],
                },
                provenance: turn_provenance,
            });
        }
    }
    Ok(output)
}

fn merge_adjacent_speaker_turns(
    segments: Vec<BlockNode>,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut output = Vec::<BlockNode>::new();
    output.try_reserve(segments.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "speaker turn merge allocation failed".into(),
    })?;
    for node in segments {
        if output.last_mut().is_some_and(|previous| can_merge_speaker_turns(previous, &node)) {
            let previous = output.last_mut().expect("checked previous speaker turn");
            merge_speaker_turn(previous, node)?;
        } else {
            output.push(node);
        }
    }
    Ok(output)
}

fn can_merge_speaker_turns(left: &BlockNode, right: &BlockNode) -> bool {
    let Block::TimedSegment { range: left_range, speaker: left_speaker, .. } = &left.block else {
        return false;
    };
    let Block::TimedSegment { range: right_range, speaker: right_speaker, .. } = &right.block
    else {
        return false;
    };
    left_speaker.is_some()
        && left_speaker == right_speaker
        && right_range.start_ms >= left_range.end_ms
        && right_range.start_ms - left_range.end_ms <= SAME_SPEAKER_MERGE_GAP_MS
}

fn merge_speaker_turn(left: &mut BlockNode, right: BlockNode) -> Result<(), ConversionError> {
    let Block::TimedSegment {
        range: right_range,
        speaker: _,
        speaker_confidence: right_confidence,
        tokens: right_tokens,
        content: right_content,
    } = right.block
    else {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "speaker turn merge received a non-timed node".into(),
        });
    };
    let Block::TimedSegment {
        range: left_range,
        speaker: _,
        speaker_confidence: left_confidence,
        tokens: left_tokens,
        content: left_content,
    } = &mut left.block
    else {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "speaker turn merge received a non-timed node".into(),
        });
    };
    let left_weight = f32::from(u16::try_from(left_tokens.len()).unwrap_or(u16::MAX).max(1));
    let right_weight = f32::from(u16::try_from(right_tokens.len()).unwrap_or(u16::MAX).max(1));
    *left_confidence = match (*left_confidence, right_confidence) {
        (Some(left), Some(right)) => {
            Some((left * left_weight + right * right_weight) / (left_weight + right_weight))
        }
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    left_tokens.try_reserve(right_tokens.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "speaker turn token merge allocation failed".into(),
    })?;
    left_tokens.extend(right_tokens);
    let [Inline::Text { value: left_text, marks: left_marks }] = left_content.as_slice() else {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "speaker turn merge received invalid timed content".into(),
        });
    };
    let [Inline::Text { value: right_text, marks: right_marks }] = right_content.as_slice() else {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "speaker turn merge received invalid timed content".into(),
        });
    };
    if !left_marks.is_empty() || !right_marks.is_empty() {
        return Err(ConversionError::Ai {
            provider: PROVIDER_ID.into(),
            detail: "speaker turn merge received marked timed content".into(),
        });
    }
    let mut merged_text = String::new();
    merged_text.try_reserve(left_text.len().saturating_add(right_text.len())).map_err(|_| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "speaker turn content merge allocation failed".into(),
        }
    })?;
    merged_text.push_str(left_text);
    merged_text.push_str(right_text);
    *left_content = vec![Inline::Text { value: merged_text, marks: Vec::new() }];
    left_range.end_ms = right_range.end_ms;
    left.provenance.locator.time = Some(*left_range);
    Ok(())
}

impl Diarizer for LocalSpeakerDiarizer {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn diarize<'a>(
        &'a self,
        request: DiarizationRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<DiarizationResult, ConversionError>> {
        Box::pin(async move { self.diarize_inner(request, context).await })
    }
}

fn milliseconds_to_frame(milliseconds: u64, maximum: u64) -> u64 {
    milliseconds.saturating_mul(u64::from(SAMPLE_RATE)).saturating_div(1_000).min(maximum)
}

fn read_segment_samples(
    audio: &NormalizedAudio,
    start: u64,
    end: u64,
    context: &ExecutionContext,
) -> Result<Vec<f32>, ConversionError> {
    let requested = end.saturating_sub(start);
    let desired = requested.clamp(
        u64::try_from(MIN_EMBEDDING_SAMPLES).unwrap_or(16_000),
        u64::try_from(MAX_EMBEDDING_SAMPLES).unwrap_or(160_000),
    );
    let context_before = desired.saturating_sub(requested) / 2;
    let centered_start = start.saturating_sub(context_before);
    let available_start = centered_start.min(audio.frames.saturating_sub(desired));
    let available_end = available_start.saturating_add(desired).min(audio.frames);
    let window = audio.read_mono_s16le(available_start, available_end, context)?;
    let mut samples = Vec::new();
    samples.try_reserve_exact(window.bytes().len() / 2).map_err(|_| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "speaker sample allocation failed".into(),
        }
    })?;
    for bytes in window.bytes().chunks_exact(2) {
        samples.push(f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0);
    }
    if samples.len() < MIN_EMBEDDING_SAMPLES {
        samples.resize(MIN_EMBEDDING_SAMPLES, 0.0);
    }
    Ok(samples)
}

#[derive(Clone, Copy)]
struct Complex {
    real: f32,
    imag: f32,
}

fn fft(values: &mut [Complex]) {
    let length = values.len();
    let mut reverse = 0_usize;
    for index in 1..length {
        let mut bit = length >> 1;
        while reverse & bit != 0 {
            reverse ^= bit;
            bit >>= 1;
        }
        reverse ^= bit;
        if index < reverse {
            values.swap(index, reverse);
        }
    }
    let mut span = 2;
    while span <= length {
        let angle = -2.0 * PI / span as f32;
        let root = Complex { real: angle.cos(), imag: angle.sin() };
        for offset in (0..length).step_by(span) {
            let mut factor = Complex { real: 1.0, imag: 0.0 };
            for index in 0..span / 2 {
                let even = values[offset + index];
                let odd = values[offset + index + span / 2];
                let rotated = Complex {
                    real: odd.real * factor.real - odd.imag * factor.imag,
                    imag: odd.real * factor.imag + odd.imag * factor.real,
                };
                values[offset + index] =
                    Complex { real: even.real + rotated.real, imag: even.imag + rotated.imag };
                values[offset + index + span / 2] =
                    Complex { real: even.real - rotated.real, imag: even.imag - rotated.imag };
                factor = Complex {
                    real: factor.real * root.real - factor.imag * root.imag,
                    imag: factor.real * root.imag + factor.imag * root.real,
                };
            }
        }
        span *= 2;
    }
}

fn hz_to_mel(hz: f32) -> f32 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

fn log_mel_fbank(samples: &[f32]) -> Result<Vec<f32>, ConversionError> {
    const FRAME: usize = 400;
    const SHIFT: usize = 160;
    const FFT_SIZE: usize = 512;
    const BINS: usize = FFT_SIZE / 2 + 1;
    const MEL_BINS: usize = 80;
    let frame_count = samples.len().saturating_sub(FRAME) / SHIFT + 1;
    let mut output = Vec::new();
    output.try_reserve_exact(frame_count.saturating_mul(MEL_BINS)).map_err(|_| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "speaker feature allocation failed".into(),
        }
    })?;
    let low_mel = hz_to_mel(20.0);
    let high_mel = hz_to_mel(SAMPLE_RATE as f32 / 2.0);
    let mel_step = (high_mel - low_mel) / (MEL_BINS + 1) as f32;
    let fft_bin_width = SAMPLE_RATE as f32 / FFT_SIZE as f32;
    let mut spectrum = vec![Complex { real: 0.0, imag: 0.0 }; FFT_SIZE];
    for frame_index in 0..frame_count {
        spectrum.fill(Complex { real: 0.0, imag: 0.0 });
        let start = frame_index * SHIFT;
        let mean = samples[start..start + FRAME].iter().sum::<f32>() / FRAME as f32;
        for index in 0..FRAME {
            let current = samples[start + index] - mean;
            let previous = if index == 0 { current } else { samples[start + index - 1] - mean };
            let hann = 0.5 - 0.5 * (2.0 * PI * index as f32 / (FRAME - 1) as f32).cos();
            let povey = hann.powf(0.85);
            spectrum[index].real = (current - 0.97 * previous) * povey;
        }
        fft(&mut spectrum);
        let powers = spectrum[..BINS]
            .iter()
            .map(|value| value.real.mul_add(value.real, value.imag * value.imag))
            .collect::<Vec<_>>();
        for mel in 0..MEL_BINS {
            let left = low_mel + mel_step * mel as f32;
            let center = left + mel_step;
            let right = center + mel_step;
            let mut energy = 0.0_f32;
            for (bin, value) in powers.iter().enumerate() {
                let frequency_mel = hz_to_mel(bin as f32 * fft_bin_width);
                let weight = if frequency_mel > left && frequency_mel <= center {
                    (frequency_mel - left) / (center - left)
                } else if frequency_mel > center && frequency_mel < right {
                    (right - frequency_mel) / (right - center)
                } else {
                    0.0
                };
                energy += *value * weight;
            }
            output.push(energy.max(f32::EPSILON).ln());
        }
    }
    for bin in 0..MEL_BINS {
        let mean = (0..frame_count).map(|frame| output[frame * MEL_BINS + bin]).sum::<f32>()
            / frame_count as f32;
        for frame in 0..frame_count {
            output[frame * MEL_BINS + bin] -= mean;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedding(axis: usize, sign: f32) -> Vec<f32> {
        let mut values = vec![0.0; EMBEDDING_DIMENSION];
        values[axis] = sign;
        values
    }

    #[test]
    fn clustering_is_stable_bounded_and_keeps_ambiguous_samples_low_confidence() {
        let mut state = OnlineCosineClustering::new(None, 2).unwrap();
        assert_eq!(state.assign(&embedding(0, 1.0)).unwrap().0, "speaker-1");
        assert_eq!(state.assign(&embedding(1, 1.0)).unwrap().0, "speaker-2");
        assert_eq!(state.assign(&embedding(0, 1.0)).unwrap().0, "speaker-1");
        let mut ambiguous = embedding(0, 1.0);
        ambiguous[1] = 1.0;
        normalize(&mut ambiguous);
        let (speaker, confidence) = state.assign(&ambiguous).unwrap();
        assert!(["speaker-1", "speaker-2"].contains(&speaker.as_str()));
        assert!(confidence < 0.5);
        assert_eq!(state.clusters.len(), 2);
        assert!(state.assign(&vec![0.0; EMBEDDING_DIMENSION]).is_none());

        let checkpoint = state.checkpoint();
        let restored = OnlineCosineClustering::from_checkpoint(None, 2, checkpoint).unwrap();
        assert_eq!(restored.clusters.len(), 2);
    }

    #[test]
    fn fixed_and_automatic_speaker_bounds_are_validated() {
        assert!(OnlineCosineClustering::new(Some(0), 16).is_err());
        assert!(OnlineCosineClustering::new(Some(17), 16).is_err());
        assert!(OnlineCosineClustering::new(None, 65).is_err());
        let mut state = OnlineCosineClustering::new(Some(1), 16).unwrap();
        assert_eq!(state.assign(&embedding(0, 1.0)).unwrap().0, "speaker-1");
        let (speaker, confidence) = state.assign(&embedding(1, 1.0)).unwrap();
        assert_eq!(speaker, "speaker-1");
        assert_eq!(confidence, 0.5);
    }

    #[test]
    fn expected_speaker_count_does_not_force_a_new_cluster() {
        let mut state = OnlineCosineClustering::new(Some(2), 16).unwrap();
        assert_eq!(state.assign(&embedding(0, 1.0)).unwrap().0, "speaker-1");
        let mut similar = embedding(0, 0.48);
        similar[1] = (1.0_f32 - 0.48_f32.powi(2)).sqrt();
        let (speaker, _) = state.assign(&similar).unwrap();
        assert_eq!(speaker, "speaker-1");
        assert_eq!(state.clusters.len(), 1);
    }

    #[test]
    fn automatic_speaker_count_avoids_one_off_cluster_fragmentation() {
        let mut state = OnlineCosineClustering::new(None, 16).unwrap();
        assert_eq!(state.assign(&embedding(0, 1.0)).unwrap().0, "speaker-1");
        let mut similar = embedding(0, 0.37);
        similar[1] = (1.0_f32 - 0.37_f32.powi(2)).sqrt();
        let (speaker, _) = state.assign(&similar).unwrap();
        assert_eq!(speaker, "speaker-1");
        assert_eq!(state.clusters.len(), 1);
    }

    #[test]
    fn automatic_singletons_require_follow_up_evidence() {
        let mut state = OnlineCosineClustering::new(None, 16).unwrap();
        state.assign(&embedding(0, 1.0)).unwrap();
        state.assign(&embedding(0, 1.0)).unwrap();
        assert_eq!(state.assign(&embedding(1, 1.0)).unwrap().0, "speaker-2");
        assert!(state.automatic_singleton_remap().is_empty());
        assert_eq!(state.assign(&embedding(2, 1.0)).unwrap().0, "speaker-3");
        assert_eq!(state.automatic_singleton_remap().len(), 1);
        state.assign(&embedding(1, 1.0)).unwrap();
        assert!(state.automatic_singleton_remap().is_empty());

        let mut expected = OnlineCosineClustering::new(Some(2), 16).unwrap();
        expected.assign(&embedding(0, 1.0)).unwrap();
        expected.assign(&embedding(1, 1.0)).unwrap();
        assert!(expected.automatic_singleton_remap().is_empty());
    }

    #[test]
    fn voice_probability_uses_speech_frames_instead_of_diluting_them_with_pauses() {
        let mut probabilities = vec![0.02; 12];
        probabilities.extend([0.91, 0.88, 0.84, 0.81]);
        let score = robust_voice_probability(&mut probabilities);
        assert!((score - 0.86).abs() < 0.001);
        assert_eq!(robust_voice_probability(&mut []), 0.0);
    }

    #[test]
    fn token_windows_are_balanced_without_a_short_tail() {
        let token = |start_ms, end_ms| TimedToken {
            range: TimeRange { start_ms, end_ms },
            text: "x".into(),
            confidence: Some(0.9),
            speaker: None,
            speaker_confidence: None,
        };
        let range = TimeRange { start_ms: 4_260, end_ms: 10_200 };
        let tokens = vec![
            token(4_260, 4_690),
            token(4_690, 5_340),
            token(5_450, 6_720),
            token(6_740, 9_410),
            token(9_490, 10_200),
        ];
        let turns = token_turns(&tokens, range);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].range, TimeRange { start_ms: 4_260, end_ms: 6_720 });
        assert_eq!(turns[1].range, TimeRange { start_ms: 6_740, end_ms: 10_200 });
        assert!(turns.iter().all(|turn| turn.range.end_ms - turn.range.start_ms >= 2_000));
    }

    #[test]
    fn fbank_is_finite_and_has_the_expected_contract() {
        let samples = (0..16_000)
            .map(|index| (2.0 * PI * 440.0 * index as f32 / SAMPLE_RATE as f32).sin() * 0.2)
            .collect::<Vec<_>>();
        let features = log_mel_fbank(&samples).unwrap();
        assert_eq!(features.len() % 80, 0);
        assert!(!features.is_empty());
        assert!(features.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn fbank_matches_kaldi_native_reference() {
        let samples = (0..16_000)
            .map(|index| {
                let phase = index as f32 / 16_000.0;
                0.23 * (2.0 * PI * 233.0 * phase).sin() + 0.11 * (2.0 * PI * 817.0 * phase).cos()
            })
            .collect::<Vec<_>>();
        let features = log_mel_fbank(&samples).unwrap();
        let selected = [
            features[0],
            features[1],
            features[80],
            features[81],
            features[20 * 80],
            features[20 * 80 + 1],
            features[97 * 80],
            features[97 * 80 + 1],
        ];
        let reference = [
            0.195_941_9,
            0.439_239_5,
            1.190_955_6,
            0.282_332_4,
            -1.781_001,
            -0.171_912_2,
            -0.588_023_2,
            0.341_694_8,
        ];
        assert!(
            selected
                .into_iter()
                .zip(reference)
                .all(|(actual, expected)| (actual - expected).abs() < 0.003)
        );
    }

    #[test]
    fn token_time_ranges_split_speaker_turns_without_rewriting_text() {
        let token = |start_ms, end_ms, text: &str, speaker: Option<&str>| TimedToken {
            range: TimeRange { start_ms, end_ms },
            text: text.into(),
            confidence: Some(0.9),
            speaker: speaker.map(str::to_owned),
            speaker_confidence: speaker.map(|_| 0.8),
        };
        let range = TimeRange { start_ms: 0, end_ms: 3_000 };
        let segments = vec![BlockNode {
            id: NodeId("segment-1".into()),
            block: Block::TimedSegment {
                range,
                speaker: None,
                speaker_confidence: None,
                tokens: vec![
                    token(0, 1_000, "Hello", Some("speaker-1")),
                    token(1_000, 2_000, " there", Some("speaker-2")),
                    token(2_000, 3_000, ".", None),
                ],
                content: vec![Inline::Text { value: "Hello there.".into(), marks: Vec::new() }],
            },
            provenance: into_markdown_core::Provenance {
                kind: into_markdown_core::ProvenanceKind::AiProvider,
                provider: "test/model".into(),
                locator: into_markdown_core::SourceLocator {
                    time: Some(range),
                    ..into_markdown_core::SourceLocator::default()
                },
                confidence: Some(0.9),
            },
        }];
        let split = split_token_speaker_turns(segments).unwrap();
        assert_eq!(split.len(), 3);
        let values = split
            .iter()
            .map(|node| match &node.block {
                Block::TimedSegment { speaker, content, .. } => {
                    let Inline::Text { value, .. } = &content[0] else { panic!("text") };
                    (speaker.as_deref(), value.as_str())
                }
                _ => panic!("timed segment"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![(Some("speaker-1"), "Hello"), (Some("speaker-2"), " there"), (None, "."),]
        );
        assert_eq!(values.iter().map(|(_, text)| *text).collect::<String>(), "Hello there.");
    }

    #[test]
    fn single_speaker_gaps_are_filled_and_nearby_segments_merge() {
        let token = |start_ms, end_ms, text: &str, speaker: Option<&str>| TimedToken {
            range: TimeRange { start_ms, end_ms },
            text: text.into(),
            confidence: Some(0.9),
            speaker: speaker.map(str::to_owned),
            speaker_confidence: speaker.map(|_| 0.8),
        };
        let node = |id: &str, range: TimeRange, tokens: Vec<TimedToken>, text: &str| BlockNode {
            id: NodeId(id.into()),
            block: Block::TimedSegment {
                range,
                speaker: None,
                speaker_confidence: None,
                tokens,
                content: vec![Inline::Text { value: text.into(), marks: Vec::new() }],
            },
            provenance: into_markdown_core::Provenance {
                kind: into_markdown_core::ProvenanceKind::AiProvider,
                provider: "test/model".into(),
                locator: into_markdown_core::SourceLocator {
                    time: Some(range),
                    ..into_markdown_core::SourceLocator::default()
                },
                confidence: Some(0.9),
            },
        };
        let mut segments = vec![
            node(
                "segment-1",
                TimeRange { start_ms: 0, end_ms: 2_000 },
                vec![
                    token(0, 900, "We", Some("speaker-1")),
                    token(900, 2_000, " should meet", None),
                ],
                "We should meet",
            ),
            node(
                "segment-2",
                TimeRange { start_ms: 2_100, end_ms: 3_000 },
                vec![token(2_100, 3_000, " tomorrow.", None)],
                " tomorrow.",
            ),
        ];
        repair_unassigned_speech(&mut segments).unwrap();
        let merged =
            merge_adjacent_speaker_turns(split_token_speaker_turns(segments).unwrap()).unwrap();
        assert_eq!(merged.len(), 1);
        assert!(
            into_markdown_core::Document {
                blocks: merged.clone(),
                ..into_markdown_core::Document::default()
            }
            .validate()
            .is_ok()
        );
        let Block::TimedSegment { range, speaker, tokens, content, .. } = &merged[0].block else {
            panic!("timed segment")
        };
        assert_eq!(*range, TimeRange { start_ms: 0, end_ms: 3_000 });
        assert_eq!(speaker.as_deref(), Some("speaker-1"));
        assert!(tokens.iter().all(|token| token.speaker.as_deref() == Some("speaker-1")));
        let text = content
            .iter()
            .map(|inline| match inline {
                Inline::Text { value, .. } => value.as_str(),
                _ => panic!("text"),
            })
            .collect::<String>();
        assert_eq!(text, "We should meet tomorrow.");
        assert_eq!(merged[0].provenance.locator.time, Some(*range));

        let mut noncontiguous = merged.clone();
        let Block::TimedSegment { speaker, tokens, .. } = &mut noncontiguous[0].block else {
            panic!("timed segment")
        };
        *speaker = Some("speaker-5".into());
        for token in tokens {
            token.speaker = Some("speaker-5".into());
        }
        compact_speaker_labels(&mut noncontiguous).unwrap();
        let Block::TimedSegment { speaker, .. } = &noncontiguous[0].block else {
            panic!("timed segment")
        };
        assert_eq!(speaker.as_deref(), Some("speaker-1"));
    }
}
