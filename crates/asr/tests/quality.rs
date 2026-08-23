use futures::executor::block_on;
use into_markdown_asr::{WhisperConfig, WhisperSmallTranscriber};
use into_markdown_core::{
    AsrOptions, Block, ExecutionContext, ExecutionOptions, Inline, ResourceLimits, Transcriber,
    TranscriptionRequest,
};
use into_markdown_ffmpeg::{FfmpegRuntime, MediaLimits};
use into_markdown_ocr::ModelManager;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    model: ModelAuthority,
    normalization: serde_json::Value,
    noise: NoiseAuthority,
    fixtures: Vec<FixtureAuthority>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelAuthority {
    bundle: String,
    bytes: u64,
    sha256: String,
    runtime: String,
    decoding_strategy: String,
    candidate_count: u32,
    maximum_threads: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoiseAuthority {
    algorithm: String,
    seed: u64,
    snr_db: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureAuthority {
    id: String,
    path: String,
    bytes: u64,
    sha256: String,
    language: String,
    reference: String,
    accepted_references: Vec<String>,
    clear_maximum_error_rate: f64,
    noise_maximum_error_rate: f64,
    source_url: String,
    source_revision: String,
    license: String,
    attribution: String,
}

#[derive(Serialize)]
struct QualityReport {
    schema_version: u32,
    authority_sha256: String,
    model: String,
    model_bytes: u64,
    runtime: String,
    decoding_strategy: String,
    candidate_count: u32,
    maximum_threads: u16,
    noise_algorithm: String,
    noise_seed: u64,
    snr_db: f64,
    cases: Vec<CaseReport>,
}

#[derive(Serialize)]
struct CaseReport {
    fixture: String,
    condition: &'static str,
    language: String,
    metric: &'static str,
    reference: String,
    reference_sha256: String,
    candidate_reference_sha256: Vec<String>,
    reference_units: usize,
    errors: usize,
    error_rate: f64,
    maximum_error_rate: f64,
    transcript: String,
}

#[test]
fn metric_and_threshold_fail_closed() {
    assert_eq!(levenshtein(&['a', 'b'], &['a', 'x', 'b']), 1);
    assert_eq!(zh_units("习，近 平！"), vec!['习', '近', '平']);
    assert_eq!(en_units("Ask, NOT 2026."), vec!["ask", "not", "2026"]);
    assert!(en_units("").is_empty());
    assert!(threshold(1, 3, 0.25).is_err());
    assert!(threshold(0, 3, 0.15).is_ok());
    assert!(threshold(0, 0, 0.15).is_err());
    assert!(threshold(0, 3, f64::NAN).is_err());
    let (errors, units, selected) =
        minimum_zh_error("習近平", &["习近平".to_owned(), "習近平".to_owned()]).unwrap();
    assert_eq!((errors, units, selected), (0, 3, 1));
    let (errors, _, _) =
        minimum_zh_error("習近帄", &["习近平".to_owned(), "習近平".to_owned()]).unwrap();
    assert_eq!(errors, 1, "a non-equivalent character must remain an error");
    assert!(minimum_zh_error("习近平", &[]).is_err());
}

#[test]
#[ignore = "requires the pinned Whisper model and audited FFmpeg runtime"]
fn whisper_small_multilingual_quality() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let authority_bytes = std::fs::read(fixture_root.join("asr-quality-authority.json")).unwrap();
    let authority: Authority = serde_json::from_slice(&authority_bytes).unwrap();
    assert_eq!(authority.schema_version, 3);
    assert_eq!(authority.model.decoding_strategy, "greedy");
    assert_eq!(authority.model.candidate_count, 1);
    assert_eq!(authority.model.bytes, 487_601_967);
    assert!(authority.model.runtime.contains("whisper.cpp"));
    assert_eq!(authority.noise.algorithm, "lcg-white-noise-v1");
    assert!(authority.normalization.is_object());

    let model_root = required_path("INTO_MD_ASR_MODEL_ROOT");
    let ffmpeg_root = required_path("INTO_MD_ASR_FFMPEG_ROOT");
    let ffmpeg_executable = required_path("INTO_MD_ASR_FFMPEG_EXECUTABLE");
    let ffmpeg_authority = required_path("INTO_MD_ASR_FFMPEG_AUTHORITY");
    let manager = Arc::new(ModelManager::embedded(model_root, None).unwrap());
    let runtime = Arc::new(
        FfmpegRuntime::load(
            &ffmpeg_root,
            &ffmpeg_executable,
            &std::fs::read(ffmpeg_authority).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(authority.model.bundle, "whisper-small-multilingual");
    let mut options = AsrOptions::default();
    options.max_threads = authority.model.maximum_threads;
    let transcriber = WhisperSmallTranscriber::new(
        Arc::clone(&manager),
        Arc::clone(&runtime),
        WhisperConfig::try_from(&options).unwrap(),
    )
    .unwrap();
    let verified = manager
        .verified_runtime_path(
            &authority.model.bundle,
            "model",
            &ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default()),
        )
        .unwrap();
    assert_eq!(verified.sha256, authority.model.sha256);
    assert_eq!(verified.size, 487_601_967);
    assert_eq!(verified.size, authority.model.bytes);

    let mut cases = Vec::new();
    let mut failures = Vec::new();
    for fixture in &authority.fixtures {
        assert!(!fixture.source_url.is_empty());
        assert!(!fixture.source_revision.is_empty());
        assert!(!fixture.license.is_empty());
        assert!(!fixture.attribution.is_empty());
        let clear = std::fs::read(fixture_root.join(&fixture.path)).unwrap();
        assert_eq!(u64::try_from(clear.len()).unwrap(), fixture.bytes);
        assert_eq!(format!("{:x}", Sha256::digest(&clear)), fixture.sha256);
        match evaluate(&transcriber, fixture, "clear", &clear, fixture.clear_maximum_error_rate) {
            Ok(report) => cases.push(report),
            Err((report, failure)) => {
                cases.push(report);
                failures.push(failure);
            }
        }
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let pcm = runtime
            .normalize(
                &clear,
                MediaLimits {
                    max_input_bytes: fixture.bytes,
                    max_duration_ms: Some(60_000),
                    sample_rate: 16_000,
                    channels: 1,
                    ..MediaLimits::default()
                },
                &context,
            )
            .unwrap();
        let noisy = noisy_wav(
            pcm.samples(),
            pcm.sample_rate,
            authority.noise.seed ^ stable_seed(&fixture.id),
            authority.noise.snr_db,
        );
        match evaluate(&transcriber, fixture, "noise", &noisy, fixture.noise_maximum_error_rate) {
            Ok(report) => cases.push(report),
            Err((report, failure)) => {
                cases.push(report);
                failures.push(failure);
            }
        }
    }
    cases.sort_by(|left, right| {
        (&left.fixture, left.condition).cmp(&(&right.fixture, right.condition))
    });
    let unique_cases: BTreeSet<_> =
        cases.iter().map(|case| (case.fixture.as_str(), case.condition)).collect();
    assert_eq!(cases.len(), 4, "quality authority must produce exactly four cases");
    assert_eq!(unique_cases.len(), 4, "quality cases must be unique");
    let report = QualityReport {
        schema_version: 3,
        authority_sha256: format!(
            "{:x}",
            Sha256::digest(String::from_utf8(authority_bytes).unwrap().replace("\r\n", "\n"))
        ),
        model: format!("{}@sha256:{}", authority.model.bundle, authority.model.sha256),
        model_bytes: authority.model.bytes,
        runtime: authority.model.runtime,
        decoding_strategy: authority.model.decoding_strategy,
        candidate_count: authority.model.candidate_count,
        maximum_threads: authority.model.maximum_threads,
        noise_algorithm: authority.noise.algorithm,
        noise_seed: authority.noise.seed,
        snr_db: authority.noise.snr_db,
        cases,
    };
    let encoded = serde_json::to_vec_pretty(&report).unwrap();
    if let Some(path) = std::env::var_os("INTO_MD_ASR_QUALITY_REPORT") {
        std::fs::write(path, &encoded).unwrap();
    }
    println!("{}", String::from_utf8(encoded).unwrap());
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn evaluate(
    transcriber: &WhisperSmallTranscriber,
    fixture: &FixtureAuthority,
    condition: &'static str,
    media: &[u8],
    maximum: f64,
) -> Result<CaseReport, (CaseReport, String)> {
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let result = block_on(transcriber.transcribe(
        TranscriptionRequest {
            media,
            media_type: if media.starts_with(b"RIFF") { "audio/wav" } else { "audio/ogg" },
            language: None,
        },
        &context,
    ))
    .unwrap();
    assert_eq!(result.language.as_deref(), Some(fixture.language.as_str()));
    assert!(
        result
            .language_confidence
            .is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
    );
    let mut previous_end = 0;
    let mut transcript = String::new();
    for node in &result.segments {
        assert!(
            node.provenance
                .confidence
                .is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        );
        let Block::TimedSegment { range, content, .. } = &node.block else {
            panic!("non-timed ASR node")
        };
        assert!(range.start_ms >= previous_end && range.end_ms > range.start_ms);
        previous_end = range.end_ms;
        for inline in content {
            if let Inline::Text { value, .. } = inline {
                transcript.push_str(value);
                transcript.push(' ');
            }
        }
    }
    let mut references = Vec::with_capacity(fixture.accepted_references.len() + 1);
    references.push(fixture.reference.clone());
    references.extend(fixture.accepted_references.iter().cloned());
    let (errors, units, selected) = if fixture.language == "zh" {
        minimum_zh_error(&transcript, &references).expect("authority references are non-empty")
    } else {
        assert!(fixture.accepted_references.is_empty());
        let reference = en_units(&references[0]);
        let actual = en_units(&transcript);
        (levenshtein(&reference, &actual), reference.len(), 0)
    };
    let selected_reference = references[selected].clone();
    let evaluation = threshold(errors, units, maximum);
    let rate = evaluation.as_ref().copied().unwrap_or_else(|rate| *rate);
    let report = CaseReport {
        fixture: fixture.id.clone(),
        condition,
        language: fixture.language.clone(),
        metric: if fixture.language == "zh" { "cer" } else { "wer" },
        reference_sha256: format!("{:x}", Sha256::digest(selected_reference.as_bytes())),
        candidate_reference_sha256: references
            .iter()
            .map(|reference| format!("{:x}", Sha256::digest(reference.as_bytes())))
            .collect(),
        reference: selected_reference,
        reference_units: units,
        errors,
        error_rate: rate,
        maximum_error_rate: maximum,
        transcript: transcript.trim().to_owned(),
    };
    match evaluation {
        Ok(_) => Ok(report),
        Err(rate) => Err((
            report,
            format!(
                "{} {condition} error rate {rate:.6} exceeds {maximum:.6}; transcript={transcript:?}",
                fixture.id
            ),
        )),
    }
}

fn minimum_zh_error(actual: &str, references: &[String]) -> Result<(usize, usize, usize), ()> {
    let actual = zh_units(actual);
    references
        .iter()
        .enumerate()
        .filter_map(|(index, reference)| {
            let reference = zh_units(reference);
            (!reference.is_empty())
                .then(|| (levenshtein(&reference, &actual), reference.len(), index))
        })
        .min_by_key(|&(errors, units, index)| (errors, units, index))
        .ok_or(())
}

fn threshold(errors: usize, units: usize, maximum: f64) -> Result<f64, f64> {
    if units == 0 || !maximum.is_finite() || !(0.0..=1.0).contains(&maximum) {
        return Err(f64::INFINITY);
    }
    let rate = errors as f64 / units as f64;
    if rate <= maximum { Ok(rate) } else { Err(rate) }
}

fn zh_units(value: &str) -> Vec<char> {
    value.chars().filter(|character| character.is_alphanumeric()).collect()
}

fn en_units(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn levenshtein<T: Eq>(expected: &[T], actual: &[T]) -> usize {
    let mut previous: Vec<usize> = (0..=actual.len()).collect();
    let mut current = vec![0; actual.len() + 1];
    for (row, expected_item) in expected.iter().enumerate() {
        current[0] = row + 1;
        for (column, actual_item) in actual.iter().enumerate() {
            current[column + 1] = (current[column] + 1)
                .min(previous[column + 1] + 1)
                .min(previous[column] + usize::from(expected_item != actual_item));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[actual.len()]
}

fn noisy_wav(pcm: &[u8], sample_rate: u32, mut state: u64, snr_db: f64) -> Vec<u8> {
    assert_eq!(pcm.len() % 2, 0);
    let signal_power = pcm
        .chunks_exact(2)
        .map(|chunk| f64::from(i16::from_le_bytes([chunk[0], chunk[1]])).powi(2))
        .sum::<f64>()
        / (pcm.len() / 2).max(1) as f64;
    let noise_rms = (signal_power / 10_f64.powf(snr_db / 10.0)).sqrt();
    let mut samples = Vec::with_capacity(pcm.len());
    for chunk in pcm.chunks_exact(2) {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let unit = ((state >> 32) as u32 as f64 / u32::MAX as f64) * 2.0 - 1.0;
        let signal = f64::from(i16::from_le_bytes([chunk[0], chunk[1]]));
        let sample = (signal + unit * noise_rms * 3_f64.sqrt())
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        samples.extend_from_slice(&sample.to_le_bytes());
    }
    let data_len = u32::try_from(samples.len()).unwrap();
    let mut wav = Vec::with_capacity(samples.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

fn stable_seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn required_path(name: &str) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or_else(|| panic!("{name} is required"))
}
