//! Typed, provider-independent state for resumable long-form media conversion.

use crate::{Block, BlockNode, ConversionError, ExecutionContext, ResourceReservation, TimeRange};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Schema version for durable media-window checkpoints.
pub const MEDIA_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Processing boundary represented by a media checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaCheckpointStage {
    /// Whisper windows are still being committed.
    Transcribing,
    /// The transcript is complete and speaker turns are being committed.
    Diarizing,
}

/// Exact identity of the normalized PCM stream used by ASR and diarization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedAudioIdentity {
    /// SHA-256 of all normalized PCM bytes.
    pub sha256: String,
    /// Exact PCM frame count.
    pub frames: u64,
    /// Samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
}

/// One bounded online speaker cluster persisted between diarization chunks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSpeakerCluster {
    /// Unit-normalized speaker centroid.
    pub centroid: Vec<f32>,
    /// Saturating count of incorporated observations.
    pub observations: u16,
}

/// Complete restart state for one long-form meeting conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaCheckpoint {
    /// Checkpoint schema version.
    pub schema_version: u32,
    /// Normalized audio identity reverified after every restart.
    pub audio: NormalizedAudioIdentity,
    /// Current media pipeline boundary.
    pub stage: MediaCheckpointStage,
    /// Next Whisper window start frame. Earlier token ownership is committed.
    pub next_window_start_frame: u64,
    /// Ordered partial transcript IR.
    pub segments: Vec<BlockNode>,
    /// Stable transcriber provider ID.
    pub transcriber_provider: String,
    /// Exact transcriber model identity.
    pub transcriber_model: String,
    /// Detected or selected language.
    pub language: Option<String>,
    /// Language-detection confidence.
    pub language_confidence: Option<f32>,
    /// Stable diarizer provider ID once diarization begins.
    pub diarizer_provider: Option<String>,
    /// Exact diarization model identity once diarization begins.
    pub diarization_model: Option<String>,
    /// Number of transcript segments durably diarized.
    pub diarization_completed_segments: u32,
    /// Bounded online clustering state.
    pub speaker_clusters: Vec<MediaSpeakerCluster>,
}

impl MediaCheckpoint {
    /// Validate durable state before it is reused or replaced.
    ///
    /// # Errors
    ///
    /// Returns a recovery error for corrupt, non-finite, out-of-range, or
    /// internally inconsistent media state.
    pub fn validate(&self) -> Result<(), ConversionError> {
        let canonical_sha = self.audio.sha256.len() == 64
            && self
                .audio
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if self.schema_version != MEDIA_CHECKPOINT_SCHEMA_VERSION
            || !canonical_sha
            || self.audio.frames == 0
            || self.audio.sample_rate != 16_000
            || self.audio.channels != 1
            || self.next_window_start_frame > self.audio.frames
            || self.transcriber_provider.is_empty()
            || self.transcriber_model.is_empty()
            || self
                .language_confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || self.segments.len() > 100_000
            || usize::try_from(self.diarization_completed_segments)
                .is_ok_and(|completed| completed > self.segments.len())
            || self.speaker_clusters.len() > 64
        {
            return Err(corrupt("media checkpoint header is inconsistent"));
        }
        match self.stage {
            MediaCheckpointStage::Transcribing
                if self.diarizer_provider.is_some()
                    || self.diarization_model.is_some()
                    || self.diarization_completed_segments != 0
                    || !self.speaker_clusters.is_empty() =>
            {
                return Err(corrupt("transcription checkpoint contains diarization state"));
            }
            MediaCheckpointStage::Diarizing
                if self.next_window_start_frame != self.audio.frames
                    || self.diarizer_provider.as_deref().is_none_or(str::is_empty)
                    || self.diarization_model.as_deref().is_none_or(str::is_empty) =>
            {
                return Err(corrupt("diarization checkpoint is incomplete"));
            }
            _ => {}
        }
        for cluster in &self.speaker_clusters {
            let norm = cluster.centroid.iter().map(|value| value * value).sum::<f32>();
            if cluster.observations == 0
                || cluster.centroid.len() != 512
                || cluster.centroid.iter().any(|value| !value.is_finite())
                || !norm.is_finite()
                || !(0.8..=1.2).contains(&norm)
            {
                return Err(corrupt("speaker clustering checkpoint is invalid"));
            }
        }
        crate::ir::validate_checkpoint_blocks(&self.segments)
            .map_err(|error| corrupt(format!("media checkpoint IR is invalid: {error}")))?;
        let mut previous = TimeRange { start_ms: 0, end_ms: 0 };
        for (index, node) in self.segments.iter().enumerate() {
            let Block::TimedSegment { range, speaker, speaker_confidence, tokens, .. } =
                &node.block
            else {
                return Err(corrupt("media checkpoint contains a non-timed node"));
            };
            if self.stage == MediaCheckpointStage::Transcribing
                && (speaker.is_some()
                    || speaker_confidence.is_some()
                    || tokens
                        .iter()
                        .any(|token| token.speaker.is_some() || token.speaker_confidence.is_some()))
            {
                return Err(corrupt("transcription checkpoint contains speaker assignments"));
            }
            if index != 0 && range.start_ms < previous.end_ms {
                return Err(corrupt("media checkpoint segment time is non-monotonic"));
            }
            previous = *range;
        }
        Ok(())
    }
}

fn corrupt(detail: impl Into<String>) -> ConversionError {
    ConversionError::Recovery { reason: "corrupt", detail: detail.into() }
}

/// Memory-accounted media checkpoint loaded from durable storage.
pub struct RecoveredMediaCheckpoint {
    state: MediaCheckpoint,
    _memory: ResourceReservation,
}

impl RecoveredMediaCheckpoint {
    /// Bind decoded state to the reservation that covered its untrusted wire representation.
    #[doc(hidden)]
    #[must_use]
    pub fn new(state: MediaCheckpoint, memory: ResourceReservation) -> Self {
        Self { state, _memory: memory }
    }

    /// Move the state into a converter already running under its output-memory credit.
    #[must_use]
    pub fn into_state(self) -> MediaCheckpoint {
        self.state
    }
}

/// Durable storage seam installed only by recoverable engine execution.
#[doc(hidden)]
pub trait MediaCheckpointBackend: Send + Sync {
    /// Load the latest media checkpoint, if one exists.
    fn load(
        &self,
        context: &ExecutionContext,
    ) -> Result<Option<RecoveredMediaCheckpoint>, ConversionError>;

    /// Atomically replace the latest media checkpoint.
    fn commit(
        &self,
        checkpoint: &MediaCheckpoint,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError>;
}

pub(crate) type SharedMediaCheckpointBackend = Arc<dyn MediaCheckpointBackend>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Inline, NodeId, Provenance, ProvenanceKind, SourceLocator, TimedToken};

    fn segment(speaker: Option<&str>) -> BlockNode {
        let range = TimeRange { start_ms: 0, end_ms: 1_000 };
        BlockNode {
            id: NodeId("segment-1".into()),
            block: Block::TimedSegment {
                range,
                speaker: speaker.map(str::to_owned),
                speaker_confidence: speaker.map(|_| 0.9),
                tokens: vec![TimedToken {
                    range,
                    text: " transcript".into(),
                    confidence: Some(0.8),
                    speaker: speaker.map(str::to_owned),
                    speaker_confidence: speaker.map(|_| 0.9),
                }],
                content: vec![Inline::Text { value: " transcript".into(), marks: Vec::new() }],
            },
            provenance: Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: "test/model".into(),
                locator: SourceLocator { time: Some(range), ..SourceLocator::default() },
                confidence: Some(0.8),
            },
        }
    }

    fn checkpoint(stage: MediaCheckpointStage, segment: BlockNode) -> MediaCheckpoint {
        MediaCheckpoint {
            schema_version: MEDIA_CHECKPOINT_SCHEMA_VERSION,
            audio: NormalizedAudioIdentity {
                sha256: "a".repeat(64),
                frames: 16_000,
                sample_rate: 16_000,
                channels: 1,
            },
            stage,
            next_window_start_frame: 16_000,
            segments: vec![segment],
            transcriber_provider: "test.transcriber".into(),
            transcriber_model: "model@sha256:fixture".into(),
            language: Some("en".into()),
            language_confidence: Some(0.8),
            diarizer_provider: (stage == MediaCheckpointStage::Diarizing)
                .then(|| "test.diarizer".into()),
            diarization_model: (stage == MediaCheckpointStage::Diarizing)
                .then(|| "diarizer@sha256:fixture".into()),
            diarization_completed_segments: u32::from(stage == MediaCheckpointStage::Diarizing),
            speaker_clusters: Vec::new(),
        }
    }

    #[test]
    fn transcribing_checkpoint_rejects_speaker_assignments() {
        let error = checkpoint(MediaCheckpointStage::Transcribing, segment(Some("speaker-1")))
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("speaker assignments"));
    }

    #[test]
    fn diarizing_checkpoint_accepts_token_level_speaker_evidence() {
        checkpoint(MediaCheckpointStage::Diarizing, segment(Some("speaker-1"))).validate().unwrap();
    }
}
