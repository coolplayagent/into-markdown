//! Audited, process-isolated `FFmpeg` audio normalization.
//!
//! This crate never searches `PATH`, loads `FFmpeg` into its parent process, or
//! enables a network protocol. Products provide an absolute audited executable
//! and independently authenticated authority bytes.

use into_markdown_core::{
    ConversionError, ExecutionContext, ExecutionStage, ResourceReservation, TemporaryFile,
};
use object::{Architecture, BinaryFormat, Object, ObjectKind};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::alloc::{Layout, alloc};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use thiserror::Error;

const STDERR_LIMIT: u64 = 64 * 1024;
const AUTHORITY_LIMIT: usize = 64 * 1024;
const AUTHORITY_STRING_LIMIT: usize = 2 * 1024;
const AUTHORITY_DEPTH_LIMIT: usize = 6;
const AUTHORITY_LIST_LIMIT: usize = 32;
const DEPENDENCY_LIMIT: usize = 32;
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
const PCM_LIMIT: u64 = 512 * 1024 * 1024;
const PROCESS_MEMORY_MIN: u64 = 4 * 1024 * 1024;
const PROCESS_MEMORY_MAX: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MACOS_VIRTUAL_ADDRESS_LIMIT: u64 = 2 * 1024 * 1024 * 1024 * 1024;
#[cfg(target_os = "macos")]
const PROCESS_DATA_MAX: u64 = MACOS_VIRTUAL_ADDRESS_LIMIT;
#[cfg(all(unix, not(target_os = "macos")))]
const PROCESS_DATA_MAX: u64 = PROCESS_MEMORY_MAX;
const PIPE_CHUNK: usize = 16 * 1024;
const WORKER_STACK: usize = 128 * 1024;
const COMMAND_OVERHEAD: u64 = 64 * 1024;
const WORKER_OVERHEAD: u64 =
    (3 * WORKER_STACK + 2 * PIPE_CHUNK) as u64 + STDERR_LIMIT + COMMAND_OVERHEAD;
const RESOURCE_ERROR_MARKERS: [&[u8]; 4] = [
    b"Cannot allocate memory",
    b"Out of memory",
    b"Memory allocation failed",
    b"Not enough memory",
];
const POLL: Duration = Duration::from_millis(10);
const EXPECTED_CONFIG: [&str; 29] = [
    "--prefix=/opt/into-markdown/ffmpeg",
    "--disable-everything",
    "--disable-gpl",
    "--disable-version3",
    "--disable-nonfree",
    "--disable-network",
    "--disable-autodetect",
    "--disable-programs",
    "--enable-ffmpeg",
    "--disable-ffprobe",
    "--disable-doc",
    "--disable-debug",
    "--disable-devices",
    "--disable-avdevice",
    "--disable-swscale",
    "--enable-avutil",
    "--enable-avcodec",
    "--enable-avformat",
    "--enable-avfilter",
    "--enable-swresample",
    "--enable-protocol=file,pipe",
    "--enable-demuxer=aac,avi,flac,matroska,mov,mp3,mpegts,ogg,wav",
    "--enable-decoder=aac,flac,mp3,opus,vorbis,pcm_s8,pcm_s16be,pcm_s16le,pcm_s24be,pcm_s24le,pcm_s32be,pcm_s32le,pcm_f32be,pcm_f32le,pcm_f64be,pcm_f64le",
    "--enable-parser=aac,mpegaudio,opus,vorbis",
    "--enable-filter=aformat,aresample",
    "--enable-encoder=pcm_s16le",
    "--enable-muxer=pcm_s16le",
    "--enable-static",
    "--disable-shared",
];
const EXPECTED_CONFIG_MACOS: [&str; 2] =
    ["--extra-cflags=-mmacosx-version-min=14.0", "--extra-ldflags=-mmacosx-version-min=14.0"];
const EXPECTED_CONFIG_WINDOWS: [&str; 2] = ["--toolchain=msvc", "--disable-x86asm"];

/// Stable failures while establishing the native trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LoadError {
    /// Authority JSON was malformed or did not describe this target.
    #[error("invalid FFmpeg artifact authority")]
    Authority,
    /// Executable path was not an absolute canonical regular file.
    #[error("unsafe FFmpeg executable path")]
    UnsafePath,
    /// Executable bytes did not match the authority.
    #[error("FFmpeg executable hash mismatch")]
    HashMismatch,
    /// Tool did not report the pinned version and configuration.
    #[error("FFmpeg build configuration mismatch")]
    BuildConfiguration,
    /// Local I/O or process creation failed.
    #[error("FFmpeg runtime I/O failed")]
    Io,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    ffmpeg_version: String,
    target: String,
    executable_bytes: u64,
    executable_sha256: String,
    configure: Vec<String>,
    binary_format: String,
    binary_architecture: String,
    dependencies: Vec<String>,
    toolchain: String,
    source_sha256: String,
    source_signature_sha256: String,
    signing_key_fingerprint: String,
    build_policy_sha256: String,
    config_log_sha256: String,
    relink_bytes: u64,
    relink_sha256: String,
}

/// Resource policy for one normalization request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaLimits {
    /// Maximum encoded input bytes.
    pub max_input_bytes: u64,
    /// Optional decoded duration ceiling in integer milliseconds. Absence
    /// leaves duration bounded by output and request resource budgets.
    pub max_duration_ms: Option<u64>,
    /// Maximum conservative encoded bits per second.
    pub max_bitrate: u64,
    /// Requested and maximum output sample rate.
    pub sample_rate: u32,
    /// Requested and maximum output channels.
    pub channels: u16,
    /// Child address-space ceiling; Windows enforces this as process memory.
    /// Callers may lower but cannot exceed the compiled platform maximum.
    pub max_process_memory_bytes: u64,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_duration_ms: None,
            max_bitrate: 50_000_000,
            sample_rate: 16_000,
            channels: 1,
            max_process_memory_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Uniform signed 16-bit little-endian interleaved PCM and exact time base.
///
/// PCM ownership cannot be detached from its request memory reservation:
///
/// ```compile_fail
/// # use into_markdown_ffmpeg::PcmAudio;
/// fn detach(audio: &mut PcmAudio) {
///     let _ = std::mem::take(&mut audio.samples);
/// }
/// ```
#[derive(Debug)]
pub struct PcmAudio {
    samples: Vec<u8>,
    /// Samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Exact decoded frame count.
    pub frames: u64,
    /// Time-base numerator (one).
    pub time_base_num: u32,
    /// Time-base denominator (`sample_rate`).
    pub time_base_den: u32,
    _memory: ResourceReservation,
}

impl PartialEq for PcmAudio {
    fn eq(&self, other: &Self) -> bool {
        self.samples == other.samples
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
            && self.frames == other.frames
            && self.time_base_num == other.time_base_num
            && self.time_base_den == other.time_base_den
    }
}

impl Eq for PcmAudio {}

impl PcmAudio {
    /// Borrow interleaved signed 16-bit little-endian PCM bytes.
    #[must_use]
    pub fn samples(&self) -> &[u8] {
        &self.samples
    }

    /// Number of retained PCM bytes.
    #[must_use]
    pub fn sample_bytes(&self) -> usize {
        self.samples.len()
    }
}

/// Accounted private S16LE PCM file produced by the audited `FFmpeg` runtime.
///
/// The file is removed and its temporary-storage charge is released when this
/// value is dropped. Callers may open bounded read handles but cannot persist
/// the native decoder output accidentally.
pub struct NormalizedAudio {
    temporary: TemporaryFile,
    /// Samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Exact decoded frame count.
    pub frames: u64,
    /// SHA-256 of the complete S16LE PCM bytes.
    pub sha256: String,
}

/// One bounded PCM window whose heap ownership remains request-accounted.
pub struct PcmWindow {
    bytes: Vec<u8>,
    _memory: ResourceReservation,
}

impl PcmWindow {
    /// Borrow the exact S16LE bytes for this window.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl NormalizedAudio {
    /// Open a fresh read-only handle to the private normalized PCM file.
    ///
    /// # Errors
    /// Returns a local I/O error if the scoped file is no longer available.
    pub fn open(&self) -> Result<File, ConversionError> {
        File::open(self.temporary.path()).map_err(Into::into)
    }

    /// Read one exact mono PCM frame range without retaining the whole source.
    ///
    /// # Errors
    /// Returns a stable resource or I/O error for invalid ranges or short reads.
    pub fn read_mono_s16le(
        &self,
        start_frame: u64,
        end_frame: u64,
        context: &ExecutionContext,
    ) -> Result<PcmWindow, ConversionError> {
        if self.channels != 1 || start_frame >= end_frame || end_frame > self.frames {
            return Err(resource("mediaPcmRange"));
        }
        let start = start_frame.checked_mul(2).ok_or_else(|| resource("mediaPcmRange"))?;
        let length = end_frame
            .checked_sub(start_frame)
            .and_then(|frames| frames.checked_mul(2))
            .ok_or_else(|| resource("mediaPcmRange"))?;
        let capacity = usize::try_from(length).map_err(|_| resource("mediaPcmRange"))?;
        let memory = context.reserve_memory(length)?;
        let mut output = allocate_exact_vec(capacity).map_err(map_reader_error)?;
        output.resize(capacity, 0);
        let mut file = self.open()?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut output)?;
        Ok(PcmWindow { bytes: output, _memory: memory })
    }
}

enum NormalizationTarget {
    Memory(ResourceReservation),
    Temporary(TemporaryFile),
}

enum NormalizedOutput {
    Memory(Vec<u8>, ResourceReservation, u64),
    Temporary(TemporaryFile, u64, String),
}

impl NormalizedOutput {
    fn len(&self) -> Result<u64, ConversionError> {
        match self {
            Self::Memory(bytes, _, _) => {
                u64::try_from(bytes.len()).map_err(|_| resource("mediaPcmBytes"))
            }
            Self::Temporary(_, length, _) => Ok(*length),
        }
    }
}

struct NormalizationResult {
    output: NormalizedOutput,
    frames: u64,
}

/// A hash- and build-configuration-verified `FFmpeg` executable.
#[derive(Debug)]
pub struct FfmpegRuntime {
    executable: PathBuf,
    version: String,
    _directory: tempfile::TempDir,
}

impl FfmpegRuntime {
    /// Verify a local tool against immutable authority bytes.
    ///
    /// # Errors
    /// Returns a stable error before native media parsing when any trust check fails.
    pub fn load(
        trusted_root: &Path,
        executable: &Path,
        authority_json: &[u8],
    ) -> Result<Self, LoadError> {
        validate_authority_envelope(authority_json)?;
        let authority: Authority =
            serde_json::from_slice(authority_json).map_err(|_| LoadError::Authority)?;
        let expected_config_len = expected_config().count();
        if authority.schema_version != 1
            || authority.target != current_target().ok_or(LoadError::Authority)?
            || authority.ffmpeg_version != "8.1.2"
            || authority.executable_sha256.len() != 64
            || authority.executable_bytes == 0
            || authority.executable_bytes > BINARY_LIMIT
            || authority.configure.len() != expected_config_len
            || authority.dependencies.len() > DEPENDENCY_LIMIT
            || !authority.configure.iter().map(String::as_str).eq(expected_config())
            || authority.binary_format != expected_binary().0
            || authority.binary_architecture != expected_binary().1
            || !authority.dependencies.iter().map(String::as_str).eq(expected_dependencies())
            || authority.toolchain.is_empty()
            || authority.toolchain.len() > AUTHORITY_STRING_LIMIT
            || authority.source_sha256
                != "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c"
            || authority.source_signature_sha256
                != "0a0963fccd70597838073f3e31b20f4a4d8cc2b5e577472c9a5a1f22624246f8"
            || authority.signing_key_fingerprint != "FCF986EA15E6E293A5644F10B4322F04D67658D8"
            || authority.build_policy_sha256
                != format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("../../../third_party/ffmpeg/build-policy.json"))
                )
            || authority.config_log_sha256.len() != 64
            || authority.relink_bytes == 0
            || authority.relink_sha256.len() != 64
        {
            return Err(LoadError::Authority);
        }
        let root = trusted_root.canonicalize().map_err(|_| LoadError::UnsafePath)?;
        if !executable.is_absolute() {
            return Err(LoadError::UnsafePath);
        }
        let canonical = executable.canonicalize().map_err(|_| LoadError::UnsafePath)?;
        if canonical != executable || !canonical.starts_with(&root) {
            return Err(LoadError::UnsafePath);
        }
        let mut file = open_no_follow(&canonical)?;
        let metadata = file.metadata().map_err(|_| LoadError::Io)?;
        if !metadata.is_file() || metadata.len() != authority.executable_bytes {
            return Err(LoadError::HashMismatch);
        }
        let mut digest = Sha256::new();
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(64 * 1024).map_err(|_| LoadError::Io)?;
        buffer.resize(64 * 1024, 0);
        let mut binary = Vec::new();
        binary
            .try_reserve_exact(usize::try_from(metadata.len()).map_err(|_| LoadError::Io)?)
            .map_err(|_| LoadError::Io)?;
        loop {
            let count = file.read(&mut buffer).map_err(|_| LoadError::Io)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            if binary.len().checked_add(count).is_none_or(|size| size > binary.capacity()) {
                return Err(LoadError::HashMismatch);
            }
            binary.extend_from_slice(&buffer[..count]);
        }
        if u64::try_from(binary.len()).map_err(|_| LoadError::HashMismatch)? != metadata.len() {
            return Err(LoadError::HashMismatch);
        }
        if format!("{:x}", digest.finalize()) != authority.executable_sha256 {
            return Err(LoadError::HashMismatch);
        }
        validate_binary(&binary, &authority)?;
        let directory = tempfile::Builder::new()
            .prefix("into-md-ffmpeg-")
            .tempdir()
            .map_err(|_| LoadError::Io)?;
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let private = directory.path().join(name);
        let mut snapshot = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&private)
            .map_err(|_| LoadError::Io)?;
        snapshot.write_all(&binary).map_err(|_| LoadError::Io)?;
        snapshot.sync_all().map_err(|_| LoadError::Io)?;
        set_executable_read_only(&private)?;
        Ok(Self { executable: private, version: authority.ffmpeg_version, _directory: directory })
    }

    /// Pinned upstream version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Decode the first audio stream into bounded in-memory S16LE PCM.
    ///
    /// This compatibility path remains suitable for short media. Long-form
    /// callers should use [`Self::normalize_to_file`] so decoded duration is
    /// bounded by temporary storage rather than heap capacity.
    ///
    /// # Errors
    /// Returns stable malformed, resource, cancellation, timeout, or component errors.
    pub fn normalize(
        &self,
        input: &[u8],
        limits: MediaLimits,
        context: &ExecutionContext,
    ) -> Result<PcmAudio, ConversionError> {
        let memory = context.reserve_memory(0)?;
        let result = self.normalize_with_target(
            input,
            limits,
            context,
            NormalizationTarget::Memory(memory),
            None,
        )?;
        let NormalizedOutput::Memory(output, mut output_memory, reserved_output) = result.output
        else {
            return Err(component("workerOutput"));
        };
        let capacity = u64::try_from(output.capacity()).map_err(|_| resource("mediaMemory"))?;
        if reserved_output > capacity {
            output_memory.shrink(reserved_output - capacity)?;
        }
        Ok(PcmAudio {
            samples: output,
            sample_rate: limits.sample_rate,
            channels: limits.channels,
            frames: result.frames,
            time_base_num: 1,
            time_base_den: limits.sample_rate,
            _memory: output_memory,
        })
    }

    /// Decode the first audio stream into an accounted private S16LE PCM file.
    ///
    /// # Errors
    /// Returns stable malformed, resource, cancellation, timeout, or component errors.
    pub fn normalize_to_file(
        &self,
        input: &[u8],
        limits: MediaLimits,
        context: &ExecutionContext,
    ) -> Result<NormalizedAudio, ConversionError> {
        self.normalize_to_file_inner(input, limits, context, None)
    }

    /// Decode into private PCM while publishing processed frames before the
    /// total decoded duration is known.
    ///
    /// # Errors
    /// Returns stable malformed, resource, cancellation, timeout, or component errors.
    pub fn normalize_to_file_with_progress(
        &self,
        input: &[u8],
        limits: MediaLimits,
        context: &ExecutionContext,
        progress_message: &str,
    ) -> Result<NormalizedAudio, ConversionError> {
        if progress_message.is_empty()
            || progress_message.len() > 128
            || progress_message.chars().any(char::is_control)
        {
            return Err(component("progressMessage"));
        }
        self.normalize_to_file_inner(input, limits, context, Some(progress_message.to_owned()))
    }

    fn normalize_to_file_inner(
        &self,
        input: &[u8],
        limits: MediaLimits,
        context: &ExecutionContext,
        progress_message: Option<String>,
    ) -> Result<NormalizedAudio, ConversionError> {
        let temporary = context.temporary_file("into-md-media-pcm")?;
        let result = self.normalize_with_target(
            input,
            limits,
            context,
            NormalizationTarget::Temporary(temporary),
            progress_message,
        )?;
        let NormalizedOutput::Temporary(temporary, _, sha256) = result.output else {
            return Err(component("workerOutput"));
        };
        Ok(NormalizedAudio {
            temporary,
            sample_rate: limits.sample_rate,
            channels: limits.channels,
            frames: result.frames,
            sha256,
        })
    }

    /// Decode the first audio stream into the selected bounded target.
    ///
    /// A protocol output ceiling is calculated before launch. Actual PCM grows
    /// fallibly under request accounting, retained for the returned value's lifetime.
    /// Timeout/cancellation always kills, waits, and joins every pipe thread.
    ///
    /// # Errors
    /// Returns stable malformed, resource, cancellation, timeout, or component errors.
    #[allow(clippy::too_many_lines)]
    fn normalize_with_target(
        &self,
        input: &[u8],
        limits: MediaLimits,
        context: &ExecutionContext,
        target: NormalizationTarget,
        progress_message: Option<String>,
    ) -> Result<NormalizationResult, ConversionError> {
        validate_limits(input, limits)?;
        context.checkpoint()?;
        let frame_width = u64::from(limits.channels) * 2;
        let file_target = matches!(&target, NormalizationTarget::Temporary(_));
        let (frames, max_output) = if let Some(duration_ms) = limits.max_duration_ms {
            let frames = duration_ms
                .checked_mul(u64::from(limits.sample_rate))
                .and_then(|value| value.checked_add(999))
                .map(|value| value / 1000)
                .ok_or_else(|| resource("mediaPcmBytes"))?;
            let output_frames = frames.checked_add(1).ok_or_else(|| resource("mediaPcmBytes"))?;
            let max_output =
                output_frames.checked_mul(frame_width).ok_or_else(|| resource("mediaPcmBytes"))?;
            if !file_target && max_output > PCM_LIMIT {
                return Err(resource("mediaPcmBytes"));
            }
            (frames, max_output)
        } else {
            let available =
                if file_target { context.available_temporary_bytes() } else { PCM_LIMIT };
            let output_frames = available / frame_width;
            if output_frames <= 1 {
                return Err(resource(if file_target {
                    "max_temporary_bytes"
                } else {
                    "mediaPcmBytes"
                }));
            }
            (output_frames.saturating_sub(1), output_frames * frame_width)
        };
        let output_frames = frames.checked_add(1).ok_or_else(|| resource("mediaPcmBytes"))?;
        let input_bytes = u64::try_from(input.len()).map_err(|_| resource("mediaInputBytes"))?;
        let working_bytes =
            input_bytes.checked_add(WORKER_OVERHEAD).ok_or_else(|| resource("mediaMemory"))?;
        let _working_memory = context.reserve_memory(working_bytes)?;
        // Some common containers (notably non-fragmented M4A/MP4) keep their
        // index at the end and therefore require a seekable source. Preserve
        // the same request-scoped byte budget and cleanup guarantees as PCM
        // output instead of silently accepting only streamable variants.
        let mut encoded_input = context.temporary_file("into-md-media-input")?;
        encoded_input.write_all_checked(input)?;
        encoded_input.flush()?;
        let encoded_path = encoded_input.path().as_os_str().to_owned();
        let private = tempfile::Builder::new()
            .prefix("into-md-media-")
            .tempdir()
            .map_err(|_| component("workerLaunch"))?;
        let frame_limit = output_frames.to_string();
        let rate = limits.sample_rate.to_string();
        let channels = limits.channels.to_string();
        let mut command = Command::new(&self.executable);
        command
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-max_alloc",
                "268435456",
                "-protocol_whitelist",
                "file,pipe",
                "-threads",
                "1",
                "-filter_threads",
                "1",
                "-filter_complex_threads",
                "1",
                "-i",
            ])
            .arg(&encoded_path)
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-frames:a",
                &frame_limit,
                "-ar",
                &rate,
                "-ac",
                &channels,
                "-c:a",
                "pcm_s16le",
                "-f",
                "s16le",
                "pipe:1",
            ])
            .env_clear()
            .current_dir(private.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = spawn_limited(command, limits.max_process_memory_bytes)?;
        #[cfg(test)]
        let _active_worker = ActiveWorker::new();
        let child = &mut process.child;
        if fail_worker_stage(1) {
            return Err(component("workerPipeSetup"));
        }
        let Some(stdout) = child.stdout.take() else {
            return Err(component("workerPipeSetup"));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(component("workerPipeSetup"));
        };
        if fail_worker_stage(2) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(component("workerInput"));
        }
        let (out_tx, out_rx) = mpsc::sync_channel(1);
        let output_context = context.clone();
        let output_spawn = if fail_worker_stage(3) {
            Err(std::io::Error::other("injected stdout spawn failure"))
        } else {
            thread::Builder::new().name("ffmpeg-stdout".into()).stack_size(WORKER_STACK).spawn(
                move || {
                    let _guard = PipeWorker::new();
                    let output = match target {
                        NormalizationTarget::Memory(memory) => read_bounded_accounted(
                            stdout, max_output, memory,
                        )
                        .map(|(bytes, memory, reserved)| {
                            NormalizedOutput::Memory(bytes, memory, reserved)
                        }),
                        NormalizationTarget::Temporary(temporary) => read_bounded_temporary(
                            stdout,
                            max_output,
                            temporary,
                            frame_width,
                            &output_context,
                            progress_message.as_deref(),
                        )
                        .map(|(temporary, length, sha256)| {
                            NormalizedOutput::Temporary(temporary, length, sha256)
                        }),
                    };
                    let _ = out_tx.send(output);
                },
            )
        };
        let Ok(output_reader) = output_spawn else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(component("workerThread"));
        };
        let (err_tx, err_rx) = mpsc::sync_channel(1);
        let error_spawn = if fail_worker_stage(4) {
            Err(std::io::Error::other("injected stderr spawn failure"))
        } else {
            thread::Builder::new().name("ffmpeg-stderr".into()).stack_size(WORKER_STACK).spawn(
                move || {
                    let _guard = PipeWorker::new();
                    let _ = err_tx.send(read_bounded(stderr, STDERR_LIMIT));
                },
            )
        };
        let Ok(error_reader) = error_spawn else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_rx.recv();
            let _ = output_reader.join();
            return Err(component("workerThread"));
        };
        let status = loop {
            match child_memory_exceeded(child, limits.max_process_memory_bytes) {
                Ok(true) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_rx.recv();
                    let _ = err_rx.recv();
                    let _ = output_reader.join();
                    let _ = error_reader.join();
                    return Err(resource("mediaProcessMemory"));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_rx.recv();
                    let _ = err_rx.recv();
                    let _ = output_reader.join();
                    let _ = error_reader.join();
                    return Err(error);
                }
                Ok(false) => {}
            }
            #[cfg(test)]
            if HOLD_WORKER.load(Ordering::Acquire) {
                if let Err(error) = context.checkpoint() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_rx.recv();
                    let _ = err_rx.recv();
                    let _ = output_reader.join();
                    let _ = error_reader.join();
                    return Err(error);
                }
                thread::sleep(POLL);
                continue;
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if let Err(error) = context.checkpoint() {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = out_rx.recv();
                        let _ = err_rx.recv();
                        let _ = output_reader.join();
                        let _ = error_reader.join();
                        return Err(error);
                    }
                    thread::sleep(POLL);
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_rx.recv();
                    let _ = err_rx.recv();
                    let _ = output_reader.join();
                    let _ = error_reader.join();
                    return Err(component("workerWait"));
                }
            }
        };
        let output_join = output_reader.join();
        let error_join = error_reader.join();
        if output_join.is_err() || error_join.is_err() {
            return Err(component("workerThread"));
        }
        let output =
            out_rx.recv().map_err(|_| component("workerOutput"))?.map_err(map_reader_error)?;
        let stderr = err_rx.recv().map_err(|_| component("workerOutput"))?.unwrap_or_default();
        if !status.success() {
            match child_failure(status, &stderr) {
                ChildFailure::Resource => return Err(resource("mediaProcessMemoryOrCpu")),
                ChildFailure::Crash => return Err(component("workerCrash")),
                ChildFailure::Ordinary => {}
            }
            let detail = if stderr.windows(12).any(|v| v == b"Invalid data") {
                "invalidMedia"
            } else {
                "unsupportedOrMalformedMedia"
            };
            return Err(ConversionError::Malformed {
                part: Some("media".into()),
                detail: detail.into(),
            });
        }
        let output_len = output.len()?;
        if output_len == 0 || output_len > max_output || output_len % frame_width != 0 {
            return Err(ConversionError::Malformed {
                part: Some("audio".into()),
                detail: "invalidPcmFrameCount".into(),
            });
        }
        let actual_frames = output_len / frame_width;
        if actual_frames > frames {
            return Err(resource("mediaDuration"));
        }
        let observed_bitrate = u128::from(input_bytes)
            .checked_mul(8)
            .and_then(|v| v.checked_mul(u128::from(limits.sample_rate)))
            .map(|v| v / u128::from(actual_frames))
            .ok_or_else(|| resource("mediaBitrate"))?;
        if observed_bitrate > u128::from(limits.max_bitrate) {
            return Err(resource("mediaBitrate"));
        }
        Ok(NormalizationResult { output, frames: actual_frames })
    }
}

fn expected_config() -> impl Iterator<Item = &'static str> {
    let platform: &'static [&'static str] = if cfg!(target_os = "macos") {
        &EXPECTED_CONFIG_MACOS
    } else if cfg!(windows) {
        &EXPECTED_CONFIG_WINDOWS
    } else {
        &[]
    };
    EXPECTED_CONFIG.iter().copied().chain(platform.iter().copied())
}

fn expected_binary() -> (&'static str, &'static str) {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("mach-o", "aarch64"),
        ("linux", "x86_64") => ("elf", "x86_64"),
        ("linux", "aarch64") => ("elf", "aarch64"),
        ("windows", "x86_64") => ("pe", "x86_64"),
        _ => ("unsupported", "unsupported"),
    }
}

fn expected_dependencies() -> impl Iterator<Item = &'static str> {
    const MACOS: [&str; 4] = [
        "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
        "/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia",
        "/System/Library/Frameworks/CoreVideo.framework/Versions/A/CoreVideo",
        "/usr/lib/libSystem.B.dylib",
    ];
    const LINUX_X64: [&str; 2] = ["libc.so.6", "libm.so.6"];
    const LINUX_ARM64: [&str; 2] = ["libc.so.6", "libm.so.6"];
    const WINDOWS: [&str; 4] = ["ADVAPI32.dll", "KERNEL32.dll", "OLE32.dll", "USER32.dll"];
    let values: &'static [&'static str] = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => &MACOS,
        ("linux", "x86_64") => &LINUX_X64,
        ("linux", "aarch64") => &LINUX_ARM64,
        ("windows", "x86_64") => &WINDOWS,
        _ => &[],
    };
    values.iter().copied()
}

fn validate_binary(bytes: &[u8], authority: &Authority) -> Result<(), LoadError> {
    let file = object::File::parse(bytes).map_err(|_| LoadError::BuildConfiguration)?;
    if file.kind() != ObjectKind::Executable
        && !(file.kind() == ObjectKind::Dynamic
            && file.format() == BinaryFormat::Elf
            && file.entry() != 0)
    {
        return Err(LoadError::BuildConfiguration);
    }
    let format = match file.format() {
        BinaryFormat::Elf => "elf",
        BinaryFormat::MachO => "mach-o",
        BinaryFormat::Coff | BinaryFormat::Pe => "pe",
        _ => "unsupported",
    };
    let architecture = match file.architecture() {
        Architecture::Aarch64 => "aarch64",
        Architecture::X86_64 => "x86_64",
        _ => "unsupported",
    };
    if format != authority.binary_format || architecture != authority.binary_architecture {
        return Err(LoadError::BuildConfiguration);
    }
    let mut imports = Vec::new();
    imports.try_reserve_exact(DEPENDENCY_LIMIT).map_err(|_| LoadError::BuildConfiguration)?;
    for item in file.import_libraries().map_err(|_| LoadError::BuildConfiguration)? {
        let item = item.map_err(|_| LoadError::BuildConfiguration)?;
        if imports.len() == DEPENDENCY_LIMIT {
            return Err(LoadError::BuildConfiguration);
        }
        let name = std::str::from_utf8(item.name()).map_err(|_| LoadError::BuildConfiguration)?;
        if name.len() > AUTHORITY_STRING_LIMIT {
            return Err(LoadError::BuildConfiguration);
        }
        let mut owned = String::new();
        owned.try_reserve_exact(name.len()).map_err(|_| LoadError::BuildConfiguration)?;
        owned.push_str(name);
        imports.push(owned);
    }
    imports.sort();
    imports.dedup();
    if !dependencies_exact(&imports, &authority.dependencies) {
        return Err(LoadError::BuildConfiguration);
    }
    Ok(())
}

fn dependencies_exact(actual: &[String], expected: &[String]) -> bool {
    actual == expected
}

#[cfg(test)]
static HOLD_WORKER: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE_PIPE_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL_WORKER_STAGE: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
struct ActiveWorker;

#[cfg(test)]
impl ActiveWorker {
    fn new() -> Self {
        ACTIVE_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(test)]
impl Drop for ActiveWorker {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PipeWorker;

impl PipeWorker {
    fn new() -> Self {
        #[cfg(test)]
        ACTIVE_PIPE_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for PipeWorker {
    fn drop(&mut self) {
        #[cfg(test)]
        ACTIVE_PIPE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
fn fail_worker_stage(stage: usize) -> bool {
    FAIL_WORKER_STAGE.load(Ordering::Acquire) == stage
}

#[cfg(not(test))]
const fn fail_worker_stage(_stage: usize) -> bool {
    false
}

enum ChildFailure {
    Resource,
    Crash,
    Ordinary,
}

fn child_failure(status: std::process::ExitStatus, stderr: &[u8]) -> ChildFailure {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(libc::SIGXCPU) {
            return ChildFailure::Resource;
        }
        if status.signal().is_some() {
            return ChildFailure::Crash;
        }
    }
    #[cfg(not(unix))]
    if matches!(status.code().map(|code| code as u32), Some(0xC000_0017 | 0xC000_009A)) {
        return ChildFailure::Resource;
    }
    #[cfg(not(unix))]
    if status.code().is_some_and(|code| (code as u32) & 0xC000_0000 == 0xC000_0000) {
        return ChildFailure::Crash;
    }
    if RESOURCE_ERROR_MARKERS
        .iter()
        .any(|marker| stderr.windows(marker.len()).any(|window| window == *marker))
    {
        ChildFailure::Resource
    } else {
        ChildFailure::Ordinary
    }
}

#[cfg(target_os = "macos")]
fn child_memory_exceeded(child: &std::process::Child, limit: u64) -> Result<bool, ConversionError> {
    // SAFETY: this libc structure is plain integer data and all-zero is a valid
    // output buffer initialization for `proc_pid_rusage`.
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the child PID is live while borrowed and the output pointer has
    // the exact V2 structure layout requested by the flavor constant.
    let result = unsafe {
        libc::proc_pid_rusage(
            i32::try_from(child.id()).map_err(|_| component("workerMemoryMonitor"))?,
            libc::RUSAGE_INFO_V2,
            (&raw mut usage).cast(),
        )
    };
    if result == 0 {
        Ok(usage.ri_phys_footprint.max(usage.ri_resident_size) > limit)
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(component("workerMemoryMonitor"))
    }
}

#[cfg(not(target_os = "macos"))]
fn child_memory_exceeded(
    _child: &std::process::Child,
    _limit: u64,
) -> Result<bool, ConversionError> {
    Ok(false)
}

fn validate_authority_envelope(bytes: &[u8]) -> Result<(), LoadError> {
    if bytes.is_empty() || bytes.len() > AUTHORITY_LIMIT {
        return Err(LoadError::Authority);
    }
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    let mut array_at_depth = [false; AUTHORITY_DEPTH_LIMIT];
    let mut list_items = [0_usize; AUTHORITY_DEPTH_LIMIT];
    let mut list_expects_item = [false; AUTHORITY_DEPTH_LIMIT];
    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            } else {
                string_bytes = string_bytes.checked_add(1).ok_or(LoadError::Authority)?;
                if string_bytes > AUTHORITY_STRING_LIMIT {
                    return Err(LoadError::Authority);
                }
            }
        } else {
            match *byte {
                b'"' => {
                    if depth > 0 && array_at_depth[depth - 1] && list_expects_item[depth - 1] {
                        count_authority_list_item(
                            &mut list_items[depth - 1],
                            &mut list_expects_item[depth - 1],
                        )?;
                    }
                    in_string = true;
                    string_bytes = 0;
                }
                b'{' => {
                    if depth > 0 && array_at_depth[depth - 1] {
                        return Err(LoadError::Authority);
                    }
                    depth = depth.checked_add(1).ok_or(LoadError::Authority)?;
                    if depth > AUTHORITY_DEPTH_LIMIT {
                        return Err(LoadError::Authority);
                    }
                }
                b'[' => {
                    if depth > 0 && array_at_depth[depth - 1] {
                        return Err(LoadError::Authority);
                    }
                    depth = depth.checked_add(1).ok_or(LoadError::Authority)?;
                    if depth > AUTHORITY_DEPTH_LIMIT {
                        return Err(LoadError::Authority);
                    }
                    array_at_depth[depth - 1] = true;
                    list_items[depth - 1] = 0;
                    list_expects_item[depth - 1] = true;
                }
                b'}' => {
                    if depth == 0 || array_at_depth[depth - 1] {
                        return Err(LoadError::Authority);
                    }
                    depth -= 1;
                }
                b']' => {
                    if depth == 0 || !array_at_depth[depth - 1] {
                        return Err(LoadError::Authority);
                    }
                    array_at_depth[depth - 1] = false;
                    depth -= 1;
                }
                b',' if depth > 0 && array_at_depth[depth - 1] => {
                    list_expects_item[depth - 1] = true;
                }
                byte if depth > 0
                    && array_at_depth[depth - 1]
                    && list_expects_item[depth - 1]
                    && !byte.is_ascii_whitespace() =>
                {
                    count_authority_list_item(
                        &mut list_items[depth - 1],
                        &mut list_expects_item[depth - 1],
                    )?;
                }
                _ => {}
            }
        }
    }
    if in_string || depth != 0 {
        return Err(LoadError::Authority);
    }
    Ok(())
}

fn count_authority_list_item(count: &mut usize, expects_item: &mut bool) -> Result<(), LoadError> {
    *count = count.checked_add(1).ok_or(LoadError::Authority)?;
    if *count > AUTHORITY_LIST_LIMIT {
        return Err(LoadError::Authority);
    }
    *expects_item = false;
    Ok(())
}

fn open_no_follow(path: &Path) -> Result<File, LoadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    options.open(path).map_err(|_| LoadError::UnsafePath)
}

fn set_executable_read_only(path: &Path) -> Result<(), LoadError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o500)).map_err(|_| LoadError::Io)?;
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path).map_err(|_| LoadError::Io)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).map_err(|_| LoadError::Io)?;
    }
    Ok(())
}

fn validate_limits(input: &[u8], limits: MediaLimits) -> Result<(), ConversionError> {
    let size = u64::try_from(input.len()).map_err(|_| resource("mediaInputBytes"))?;
    if size == 0 {
        return Err(ConversionError::Malformed {
            part: Some("media".into()),
            detail: "emptyMedia".into(),
        });
    }
    if size > limits.max_input_bytes {
        return Err(resource("mediaInputBytes"));
    }
    if limits.max_duration_ms == Some(0) {
        return Err(resource("mediaDuration"));
    }
    if !(8_000..=192_000).contains(&limits.sample_rate) {
        return Err(resource("mediaSampleRate"));
    }
    if !(1..=8).contains(&limits.channels) {
        return Err(resource("mediaChannels"));
    }
    if !(PROCESS_MEMORY_MIN..=PROCESS_MEMORY_MAX).contains(&limits.max_process_memory_bytes) {
        return Err(resource("mediaProcessMemory"));
    }
    if let Some(duration_ms) = limits.max_duration_ms {
        let bitrate = size.saturating_mul(8).saturating_mul(1000) / duration_ms;
        if bitrate > limits.max_bitrate {
            return Err(resource("mediaBitrate"));
        }
    }
    Ok(())
}

fn read_bounded(mut reader: impl Read, limit: u64) -> Result<Vec<u8>, ()> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; PIPE_CHUNK];
    let mut exceeded = false;
    loop {
        let count = reader.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            return if exceeded { Err(()) } else { Ok(output) };
        }
        let next = u64::try_from(output.len())
            .map_err(|_| ())?
            .checked_add(u64::try_from(count).map_err(|_| ())?)
            .ok_or(())?;
        if next > limit {
            exceeded = true;
            continue;
        }
        if !exceeded {
            output.try_reserve_exact(count).map_err(|_| ())?;
            output.extend_from_slice(&buffer[..count]);
        }
    }
}

fn read_bounded_accounted(
    mut reader: impl Read,
    limit: u64,
    mut memory: ResourceReservation,
) -> Result<(Vec<u8>, ResourceReservation, u64), ReaderError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; PIPE_CHUNK];
    let mut reserved = 0_u64;
    loop {
        let count = reader.read(&mut buffer).map_err(|_| ReaderError::Io)?;
        if count == 0 {
            return Ok((output, memory, reserved));
        }
        let next = u64::try_from(output.len())
            .map_err(|_| ReaderError::Allocation)?
            .checked_add(u64::try_from(count).map_err(|_| ReaderError::Allocation)?)
            .ok_or(ReaderError::Allocation)?;
        if next > limit {
            return Err(ReaderError::ProtocolLimit);
        }
        let old_capacity = output.capacity();
        if output.len().checked_add(count).ok_or(ReaderError::Allocation)? > old_capacity {
            let desired = usize::try_from(next)
                .map_err(|_| ReaderError::Allocation)?
                .checked_add(PIPE_CHUNK - 1)
                .map(|value| value / PIPE_CHUNK * PIPE_CHUNK)
                .ok_or(ReaderError::Allocation)?
                .min(usize::try_from(limit).map_err(|_| ReaderError::Allocation)?);
            let prepaid = u64::try_from(desired).map_err(|_| ReaderError::Allocation)?;
            memory.grow(prepaid).map_err(ReaderError::Context)?;
            let mut replacement = match allocate_exact_vec(desired) {
                Ok(value) => value,
                Err(error) => {
                    let _ = memory.shrink(prepaid);
                    return Err(error);
                }
            };
            replacement.extend_from_slice(&output);
            output = replacement;
            let old = u64::try_from(old_capacity).map_err(|_| ReaderError::Allocation)?;
            memory.shrink(old).map_err(ReaderError::Context)?;
            reserved = prepaid;
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn read_bounded_temporary(
    mut reader: impl Read,
    limit: u64,
    mut temporary: TemporaryFile,
    frame_width: u64,
    context: &ExecutionContext,
    progress_message: Option<&str>,
) -> Result<(TemporaryFile, u64, String), ReaderError> {
    let mut buffer = [0_u8; PIPE_CHUNK];
    let mut length = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let count = reader.read(&mut buffer).map_err(|_| ReaderError::Io)?;
        if count == 0 {
            temporary.sync_all().map_err(ReaderError::Context)?;
            return Ok((temporary, length, format!("{:x}", digest.finalize())));
        }
        let count_u64 = u64::try_from(count).map_err(|_| ReaderError::Allocation)?;
        length = length.checked_add(count_u64).ok_or(ReaderError::Allocation)?;
        if length > limit {
            return Err(ReaderError::ProtocolLimit);
        }
        temporary.write_all_checked(&buffer[..count]).map_err(ReaderError::Context)?;
        digest.update(&buffer[..count]);
        if let Some(message) = progress_message {
            context
                .report(ExecutionStage::Ai, Some(length / frame_width.max(1)), None, Some(message))
                .map_err(ReaderError::Context)?;
        }
    }
}

fn allocate_exact_vec(capacity: usize) -> Result<Vec<u8>, ReaderError> {
    if capacity == 0 {
        return Ok(Vec::new());
    }
    let layout = Layout::array::<u8>(capacity).map_err(|_| ReaderError::Allocation)?;
    // SAFETY: layout is non-zero and exact; null is handled below.
    let pointer = unsafe { alloc(layout) };
    if pointer.is_null() {
        return Err(ReaderError::Allocation);
    }
    // SAFETY: the pointer exclusively owns `capacity` bytes from the global
    // allocator; initialized length remains zero until subsequent writes.
    Ok(unsafe { Vec::from_raw_parts(pointer, 0, capacity) })
}

#[derive(Debug)]
enum ReaderError {
    ProtocolLimit,
    Allocation,
    Io,
    Context(ConversionError),
}

fn map_reader_error(error: ReaderError) -> ConversionError {
    match error {
        ReaderError::ProtocolLimit => resource("mediaPcmBytes"),
        ReaderError::Allocation => resource("mediaMemory"),
        ReaderError::Io => component("workerOutput"),
        ReaderError::Context(error) => error,
    }
}

struct LimitedProcess {
    child: std::process::Child,
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl Drop for LimitedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn spawn_limited(mut command: Command, limit: u64) -> Result<LimitedProcess, ConversionError> {
    use std::os::unix::process::CommandExt;
    let address_limit = unix_address_limit(limit);
    let address = libc::rlimit { rlim_cur: address_limit, rlim_max: address_limit };
    let data_limit = address_limit.min(PROCESS_DATA_MAX);
    let data = libc::rlimit { rlim_cur: data_limit, rlim_max: data_limit };
    // Long-form media is governed by the request deadline and the cooperative
    // parent watchdog below. A fixed child CPU rlimit would turn that resource
    // policy into an undocumented meeting-duration ceiling.
    let file = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let descriptors = libc::rlimit { rlim_cur: 16, rlim_max: 16 };
    let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: this callback performs one async-signal-safe setrlimit syscall
    // sequence between fork and exec and captures only Copy values.
    unsafe {
        command.pre_exec(move || {
            if libc::setrlimit(libc::RLIMIT_AS, &raw const address) == 0
                && libc::setrlimit(libc::RLIMIT_DATA, &raw const data) == 0
                && libc::setrlimit(libc::RLIMIT_FSIZE, &raw const file) == 0
                && libc::setrlimit(libc::RLIMIT_NOFILE, &raw const descriptors) == 0
                && libc::setrlimit(libc::RLIMIT_CORE, &raw const core) == 0
            {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    command.spawn().map(|child| LimitedProcess { child }).map_err(|_| {
        if limit <= 64 * 1024 * 1024 {
            resource("mediaProcessMemoryOrCpu")
        } else {
            component("workerLimitUnavailable")
        }
    })
}

#[cfg(target_os = "macos")]
const fn unix_address_limit(_limit: u64) -> u64 {
    MACOS_VIRTUAL_ADDRESS_LIMIT
}

#[cfg(all(unix, not(target_os = "macos")))]
const fn unix_address_limit(limit: u64) -> u64 {
    limit
}

#[cfg(windows)]
fn spawn_limited(mut command: Command, limit: u64) -> Result<LimitedProcess, ConversionError> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
    }
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|_| component("workerLaunch"))?;
    // SAFETY: null security/name creates a private job object.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(component("workerLimitUnavailable"));
    }
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    info.BasicLimitInformation.ActiveProcessLimit = 1;
    info.ProcessMemoryLimit = usize::try_from(limit).map_err(|_| resource("mediaProcessMemory"))?;
    // SAFETY: handles are live, child remains suspended, and size/layout match the Win32 API.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|_| component("workerLimitUnavailable"))?,
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle()) != 0
    };
    if !ok || unsafe { NtResumeProcess(child.as_raw_handle()) } != 0 {
        // SAFETY: job is still a unique live raw handle.
        unsafe { CloseHandle(job) };
        let _ = child.kill();
        let _ = child.wait();
        return Err(component("workerLimitUnavailable"));
    }
    // SAFETY: unique ownership transfers to OwnedHandle.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    Ok(LimitedProcess { child, _job: job })
}

fn resource(limit: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: "media safety budget exceeded".into() }
}
fn component(detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable { component: "ffmpeg-lgpl".into(), detail: detail.into() }
}
fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits};

    struct CancellingReader<'a> {
        bytes: &'a [u8],
        token: CancellationToken,
    }
    impl Read for CancellingReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let count = output.len().min(self.bytes.len());
            output[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            self.token.cancel();
            Ok(count)
        }
    }

    struct SlowReader<'a>(&'a [u8]);
    impl Read for SlowReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            thread::sleep(Duration::from_millis(10));
            let count = output.len().min(self.0.len());
            output[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        }
    }
    #[test]
    fn rejects_empty_and_invalid_limits() {
        assert_eq!(
            validate_limits(&[], MediaLimits::default()).unwrap_err().code().as_str(),
            "malformed"
        );
        assert_eq!(
            validate_limits(b"x", MediaLimits { channels: 0, ..MediaLimits::default() })
                .unwrap_err()
                .code()
                .as_str(),
            "resourceLimit"
        );
    }
    #[test]
    fn bounded_reader_rejects_bomb() {
        assert!(read_bounded(&b"12345"[..], 4).is_err());
    }

    #[test]
    fn accounted_reader_preserves_typed_budget_cancellation_and_timeout() {
        let bytes = vec![7_u8; PIPE_CHUNK + 1];
        let exact_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 4 * PIPE_CHUNK as u64, ..ResourceLimits::default() },
        );
        let (rounded, lease, reserved) =
            read_bounded_accounted(&bytes[..], PCM_LIMIT, exact_context.reserve_memory(0).unwrap())
                .unwrap();
        assert_eq!(rounded.capacity(), 2 * PIPE_CHUNK);
        assert_eq!(reserved, 2 * PIPE_CHUNK as u64);
        let boundary = exact_context.reserve_memory(2 * PIPE_CHUNK as u64).unwrap();
        assert!(exact_context.reserve_memory(1).is_err());
        drop(boundary);
        drop(lease);
        drop(rounded);
        let low_memory = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
        );
        let error =
            read_bounded_accounted(&bytes[..], PCM_LIMIT, low_memory.reserve_memory(0).unwrap())
                .unwrap_err();
        assert!(matches!(
            error,
            ReaderError::Context(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));

        let token = CancellationToken::new();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let cancelled_reservation = cancelled.reserve_memory(0).unwrap();
        assert!(matches!(
            read_bounded_accounted(
                CancellingReader { bytes: &bytes, token },
                PCM_LIMIT,
                cancelled_reservation
            ),
            Err(ReaderError::Context(ConversionError::Cancelled))
        ));

        let timed = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_millis(1)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let timed_reservation = timed.reserve_memory(0).unwrap();
        assert!(matches!(
            read_bounded_accounted(SlowReader(&bytes), PCM_LIMIT, timed_reservation),
            Err(ReaderError::Context(ConversionError::Timeout))
        ));
    }

    #[test]
    fn authority_envelope_and_import_sets_are_bounded_and_exact() {
        assert_eq!(
            validate_authority_envelope(&vec![b' '; AUTHORITY_LIMIT + 1]),
            Err(LoadError::Authority)
        );
        let nested = br"[[[[[[[]]]]]]]";
        assert_eq!(validate_authority_envelope(nested), Err(LoadError::Authority));
        let oversized_list = format!("[{}]", vec![r#""x""#; AUTHORITY_LIST_LIMIT + 1].join(","));
        assert_eq!(
            validate_authority_envelope(oversized_list.as_bytes()),
            Err(LoadError::Authority)
        );
        let expected = vec!["a".to_owned(), "b".to_owned()];
        assert!(dependencies_exact(&expected, &expected));
        assert!(!dependencies_exact(&["a".to_owned()], &expected));
        assert!(!dependencies_exact(&["a".to_owned(), "b".to_owned(), "c".to_owned()], &expected));
    }

    #[test]
    #[ignore = "requires an artifact and generated authority from the manual audit"]
    #[allow(clippy::too_many_lines)]
    fn native_smoke() {
        let executable = PathBuf::from(std::env::var_os("FFMPEG_TEST_EXECUTABLE").unwrap())
            .canonicalize()
            .unwrap();
        let authority_path = PathBuf::from(std::env::var_os("FFMPEG_TEST_AUTHORITY").unwrap());
        let authority = fs::read(authority_path).unwrap();
        let disposable = tempfile::tempdir().unwrap();
        let overwrite_path = disposable.path().join("ffmpeg-overwrite");
        fs::copy(&executable, &overwrite_path).unwrap();
        let overwrite_path = overwrite_path.canonicalize().unwrap();
        let overwrite_runtime =
            FfmpegRuntime::load(disposable.path(), &overwrite_path, &authority).unwrap();
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&overwrite_path)
            .unwrap()
            .write_all(b"attacker in-place overwrite")
            .unwrap();

        let replacement_path = disposable.path().join("ffmpeg-replacement");
        fs::copy(&executable, &replacement_path).unwrap();
        let replacement_path = replacement_path.canonicalize().unwrap();
        let runtime =
            FfmpegRuntime::load(disposable.path(), &replacement_path, &authority).unwrap();
        let renamed = disposable.path().join("verified-renamed-away");
        fs::rename(&replacement_path, &renamed).unwrap();
        fs::write(&replacement_path, b"attacker rename replacement").unwrap();
        let wav = test_wav(8_000, 800);
        let context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_secs(10)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let overwritten_output = overwrite_runtime
            .normalize(
                &wav,
                MediaLimits { max_duration_ms: Some(101), ..MediaLimits::default() },
                &context,
            )
            .unwrap();
        assert_eq!(overwritten_output.frames, 1_600);
        let output = runtime
            .normalize(
                &wav,
                MediaLimits { max_duration_ms: Some(101), ..MediaLimits::default() },
                &context,
            )
            .unwrap();
        assert_eq!(output.frames, 1_600);
        assert_eq!(output.sample_bytes(), 3_200);
        assert_eq!(output.samples().len(), 3_200);
        assert!(output.samples.capacity() <= PIPE_CHUNK);
        let retained_capacity = u64::try_from(output.samples.capacity()).unwrap();
        let retained_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 2 * 1024 * 1024, ..ResourceLimits::default() },
        );
        let retained = runtime
            .normalize(
                &wav,
                MediaLimits { max_duration_ms: Some(101), ..MediaLimits::default() },
                &retained_context,
            )
            .unwrap();
        let boundary = retained_context
            .reserve_memory(2 * 1024 * 1024 - u64::try_from(retained.samples.capacity()).unwrap())
            .unwrap();
        assert!(retained_context.reserve_memory(1).is_err());
        drop(boundary);
        drop(retained);
        assert!(retained_context.reserve_memory(2 * 1024 * 1024).is_ok());
        assert!(retained_capacity <= u64::try_from(PIPE_CHUNK).unwrap());
        assert_eq!(
            validate_limits(
                &wav,
                MediaLimits {
                    max_process_memory_bytes: PROCESS_MEMORY_MAX + 1,
                    ..MediaLimits::default()
                }
            )
            .unwrap_err()
            .code()
            .as_str(),
            "resourceLimit"
        );
        let overlong = runtime
            .normalize(
                &wav,
                MediaLimits { max_duration_ms: Some(50), ..MediaLimits::default() },
                &context,
            )
            .unwrap_err();
        assert!(
            matches!(&overlong, ConversionError::ResourceLimit { limit: "mediaPcmBytes", .. }),
            "unexpected output-budget error: {overlong:?}"
        );
        let malformed =
            runtime.normalize(b"not media", MediaLimits::default(), &context).unwrap_err();
        assert_eq!(malformed.code().as_str(), "malformed");

        for stage in 1..=4 {
            FAIL_WORKER_STAGE.store(stage, Ordering::Release);
            let injected = runtime
                .normalize(
                    &wav,
                    MediaLimits { max_duration_ms: Some(101), ..MediaLimits::default() },
                    &context,
                )
                .unwrap_err();
            assert_eq!(injected.code().as_str(), "componentUnavailable");
            assert_eq!(ACTIVE_PIPE_WORKERS.load(Ordering::Acquire), 0);
        }
        FAIL_WORKER_STAGE.store(0, Ordering::Release);

        let low_memory = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
        );
        let budget_error =
            runtime.normalize(&wav, MediaLimits::default(), &low_memory).unwrap_err();
        assert!(
            matches!(
                &budget_error,
                ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }
            ),
            "unexpected memory-budget error: {budget_error:?}"
        );

        HOLD_WORKER.store(true, Ordering::Release);
        let token = CancellationToken::new();
        let cancel_context = ExecutionContext::new(
            ExecutionOptions {
                cancellation: token.clone(),
                timeout: Some(Duration::from_secs(10)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            token.cancel();
        });
        let cancellation_error =
            runtime.normalize(&wav, MediaLimits::default(), &cancel_context).unwrap_err();
        canceller.join().unwrap();
        assert_eq!(cancellation_error.code().as_str(), "cancelled");
        assert_eq!(ACTIVE_WORKERS.load(Ordering::Acquire), 0);
        assert_eq!(ACTIVE_PIPE_WORKERS.load(Ordering::Acquire), 0);

        let timeout_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_millis(30)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let timed_out =
            runtime.normalize(&wav, MediaLimits::default(), &timeout_context).unwrap_err();
        HOLD_WORKER.store(false, Ordering::Release);
        assert_eq!(timed_out.code().as_str(), "timeout");
        assert_eq!(ACTIVE_WORKERS.load(Ordering::Acquire), 0);
        assert_eq!(ACTIVE_PIPE_WORKERS.load(Ordering::Acquire), 0);

        let extreme = runtime
            .normalize(
                &wav,
                MediaLimits {
                    max_duration_ms: Some(86_400_000),
                    sample_rate: 192_000,
                    channels: 8,
                    ..MediaLimits::default()
                },
                &context,
            )
            .unwrap_err();
        assert!(matches!(extreme, ConversionError::ResourceLimit { limit: "mediaPcmBytes", .. }));

        HOLD_WORKER.store(true, Ordering::Release);
        let child_memory = runtime
            .normalize(
                &wav,
                MediaLimits {
                    max_process_memory_bytes: PROCESS_MEMORY_MIN,
                    ..MediaLimits::default()
                },
                &context,
            )
            .unwrap_err();
        HOLD_WORKER.store(false, Ordering::Release);
        assert!(
            matches!(
                &child_memory,
                ConversionError::ResourceLimit {
                    limit: "mediaProcessMemory" | "mediaProcessMemoryOrCpu",
                    ..
                }
            ),
            "unexpected child-memory error: {child_memory:?}"
        );
        if let Some(fixtures) = std::env::var_os("FFMPEG_TEST_FIXTURES") {
            for format in ["mp3", "m4a", "flac", "ogg"] {
                let bytes =
                    fs::read(PathBuf::from(&fixtures).join(format!("sample.{format}"))).unwrap();
                let decoded = runtime
                    .normalize(
                        &bytes,
                        MediaLimits { max_duration_ms: Some(30_000), ..MediaLimits::default() },
                        &context,
                    )
                    .unwrap();
                assert!(decoded.frames > 0, "{format} yielded no PCM");
                assert!(!decoded.samples().is_empty());
            }
        }
    }

    fn test_wav(rate: u32, frames: u32) -> Vec<u8> {
        let data_bytes = frames * 2;
        let mut wav = Vec::with_capacity(usize::try_from(data_bytes + 44).unwrap());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(data_bytes + 36).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            wav.extend_from_slice(&(i16::try_from(frame % 100).unwrap() * 100).to_le_bytes());
        }
        wav
    }
}
