//! Declarative command-line grammar.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;

/// Convert documents to GitHub-Flavored Markdown and structured artifacts.
#[derive(Debug, Parser)]
#[command(
    name = "into-md",
    version,
    about = "Convert documents into Markdown, structured IR, or portable bundles",
    long_about = "Convert local files, directories, standard input, and explicitly enabled remote sources. Network and AI capabilities are disabled unless authorized for the current invocation.",
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Options shared by conversion and management commands.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// Management command. With no command, inputs are converted directly.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Direct conversion options.
    #[command(flatten)]
    pub conversion: ConversionArgs,
}

/// Options accepted by every command.
#[derive(Debug, Clone, Default, Args)]
pub struct GlobalArgs {
    /// Load an additional configuration file after automatic files.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Vec<PathBuf>,

    /// Disable automatic global and project configuration discovery.
    #[arg(long, global = true)]
    pub no_config: bool,

    /// Apply a named configuration profile.
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,

    /// Human-facing help and diagnostic language.
    #[arg(long, value_enum, global = true)]
    pub language: Option<Language>,

    /// Suppress progress and informational messages.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase diagnostic verbosity; may be repeated.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Color policy for human-facing stderr output.
    #[arg(long, value_enum, global = true)]
    pub color: Option<When>,

    /// Progress display policy.
    #[arg(long, value_enum, global = true)]
    pub progress: Option<When>,

    /// Diagnostic event format written to stderr.
    #[arg(long, value_enum, global = true)]
    pub log_format: Option<LogFormat>,
}

/// Direct conversion arguments.
#[derive(Debug, Clone, Default, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ConversionArgs {
    /// Input paths, directories, URIs, or '-' for standard input.
    #[arg(value_name = "INPUT")]
    pub inputs: Vec<OsString>,

    /// Recursively traverse directory inputs.
    #[arg(short, long)]
    pub recursive: bool,

    /// Include paths matching a glob relative to each input root.
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude paths matching a glob relative to each input root.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Include hidden files during directory traversal.
    #[arg(long)]
    pub hidden: bool,

    /// Maximum number of conversions executing concurrently.
    #[arg(long, value_name = "N")]
    pub jobs: Option<NonZeroUsize>,

    /// Explicit input format for every selected input.
    #[arg(short = 'f', long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// Filename extension hint, with or without a leading dot.
    #[arg(long, value_name = "EXT")]
    pub extension: Option<String>,

    /// MIME media type hint.
    #[arg(long, value_name = "TYPE")]
    pub mime_type: Option<String>,

    /// Character encoding hint.
    #[arg(long, value_name = "CHARSET")]
    pub charset: Option<String>,

    /// Invalid character-sequence policy for plain text.
    #[arg(long, value_enum, value_name = "MODE")]
    pub encoding_errors: Option<EncodingErrorsArg>,

    /// Header-row policy for CSV and TSV.
    #[arg(long, value_enum, value_name = "MODE")]
    pub table_header: Option<TableHeaderArg>,

    /// Ragged-record policy for CSV and TSV.
    #[arg(long, value_enum, value_name = "MODE")]
    pub ragged_rows: Option<RaggedRowsArg>,

    /// Output path for one input.
    #[arg(short = 'o', long, value_name = "PATH", conflicts_with = "output_dir")]
    pub output: Option<PathBuf>,

    /// Output directory for multiple inputs or a directory input.
    #[arg(long, value_name = "DIR", conflicts_with = "output")]
    pub output_dir: Option<PathBuf>,

    /// Primary artifact kind.
    #[arg(long, value_enum)]
    pub emit: Option<EmitKind>,

    /// Directory for extracted assets.
    #[arg(long, value_name = "DIR")]
    pub assets_dir: Option<PathBuf>,

    /// Asset representation policy.
    #[arg(long, value_enum)]
    pub asset_mode: Option<AssetModeArg>,

    /// Existing-output behavior.
    #[arg(long, value_enum)]
    pub conflict: Option<ConflictPolicy>,

    /// Write a versioned machine-readable batch report.
    #[arg(long, value_name = "REPORT.json")]
    pub report: Option<PathBuf>,

    /// Expand and validate work without converting, networking, or writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Local OCR routing policy.
    #[arg(long, value_enum)]
    pub ocr: Option<OcrPolicyArg>,

    /// Local OCR model bundle ID.
    #[arg(long, value_name = "BUNDLE_ID")]
    pub ocr_model: Option<String>,

    /// OCR language hint; may be repeated.
    #[arg(long, value_name = "BCP47")]
    pub ocr_language: Vec<String>,

    /// Minimum accepted OCR confidence in the inclusive range 0..1.
    #[arg(long, value_name = "0..1")]
    pub ocr_min_confidence: Option<f32>,

    /// Local Whisper model bundle ID.
    #[arg(long, value_name = "BUNDLE_ID")]
    pub asr_model: Option<String>,

    /// ASR language hint; absence enables model detection.
    #[arg(long, value_name = "BCP47")]
    pub asr_language: Option<String>,

    /// Maximum local ASR decoder threads.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=8))]
    pub asr_threads: Option<u16>,

    /// Maximum decoded media duration accepted by ASR.
    #[arg(long, value_name = "MILLISECONDS")]
    pub asr_max_duration_ms: Option<u64>,

    /// Assign stable anonymous speaker labels in media transcripts.
    #[arg(long)]
    pub diarize: bool,

    /// Expected number of speakers; absence enables bounded automatic discovery.
    #[arg(long, value_name = "N", requires = "diarize", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub expected_speakers: Option<u16>,

    /// Set one AI capability mode as CAPABILITY=MODE; may be repeated.
    #[arg(long, value_name = "CAPABILITY=MODE")]
    pub ai: Vec<String>,

    /// Configured AI provider name.
    #[arg(long, value_name = "NAME")]
    pub ai_provider: Option<String>,

    /// Override the configured provider model for this invocation.
    #[arg(long, value_name = "MODEL")]
    pub ai_model: Option<String>,

    /// Load an AI prompt suffix from CAPABILITY=FILE; may be repeated.
    #[arg(long, value_name = "CAPABILITY=FILE")]
    pub ai_prompt: Vec<String>,

    /// Authorize remote sources and configured providers for this invocation.
    #[arg(long)]
    pub allow_network: bool,

    /// Additionally authorize loopback and private-network targets.
    #[arg(long, requires = "allow_network")]
    pub allow_private_network: bool,

    /// Restrict networking to a hostname; may be repeated.
    #[arg(long, value_name = "HOST", requires = "allow_network")]
    pub allow_host: Vec<String>,

    /// Maximum redirects per request.
    #[arg(long, value_name = "N")]
    pub max_redirects: Option<u8>,

    /// Maximum source bytes, accepting KiB, MiB, and GiB suffixes.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_input_size: Option<u64>,

    /// Maximum total decompressed bytes.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_decompressed_size: Option<u64>,

    /// Maximum archive entry count.
    #[arg(long, value_name = "N")]
    pub max_archive_entries: Option<u32>,

    /// Maximum nested archive depth.
    #[arg(long, value_name = "N")]
    pub max_archive_depth: Option<u16>,

    /// Maximum decompressed bytes for one archive member.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_archive_entry_size: Option<u64>,

    /// Maximum decompressed-to-compressed ratio for one archive member.
    #[arg(long, value_name = "N")]
    pub max_archive_compression_ratio: Option<u32>,

    /// Maximum XML or container nesting depth.
    #[arg(long, value_name = "N")]
    pub max_depth: Option<u16>,

    /// Maximum page-like units.
    #[arg(long, value_name = "N")]
    pub max_pages: Option<u32>,

    /// Maximum retained asset bytes.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_asset_size: Option<u64>,

    /// Maximum retained bytes across all assets.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_total_asset_size: Option<u64>,

    /// Total conversion timeout in milliseconds.
    #[arg(long, value_name = "MILLISECONDS", value_parser = parse_duration_ms)]
    pub timeout_ms: Option<u64>,

    /// Maximum request-scoped accounted memory.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_memory_size: Option<u64>,

    /// Maximum request-scoped temporary file bytes.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_temporary_size: Option<u64>,

    /// Maximum CSV/TSV records.
    #[arg(long, value_name = "N")]
    pub max_table_rows: Option<u64>,

    /// Maximum CSV/TSV columns.
    #[arg(long, value_name = "N")]
    pub max_table_columns: Option<u64>,

    /// Maximum CSV/TSV cells.
    #[arg(long, value_name = "N")]
    pub max_table_cells: Option<u64>,

    /// Maximum decoded bytes in one CSV/TSV field.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_field_size: Option<u64>,
}

/// Management command tree.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the loopback-only local Web service.
    Ui(UiArgs),
    /// List and inspect registered core input formats.
    Formats(FormatsArgs),
    /// Inspect and manage local OCR model bundles.
    Models(ModelsArgs),
    /// Prepare optional local runtime components.
    Setup(SetupArgs),
    /// Re-render and relabel existing meeting transcripts.
    Transcript(TranscriptArgs),
    /// Configure and inspect AI providers.
    Providers(ProvidersArgs),
    /// Inspect and manage isolated extensions.
    Plugins(PluginsArgs),
    /// Inspect and edit layered configuration.
    Config(ConfigArgs),
    /// Diagnose local configuration and runtime availability.
    Doctor(DoctorArgs),
    /// Generate shell completion definitions.
    Completions(CompletionsArgs),
    /// Print detailed build version information.
    Version(VersionArgs),
}

/// `setup` arguments.
#[derive(Debug, Args)]
pub struct SetupArgs {
    #[command(subcommand)]
    pub command: SetupCommand,
}

/// Optional component preparation operations.
#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Install and verify local media transcription components.
    Media {
        /// Allow development transport without TLS certificate validation.
        #[arg(long)]
        insecure: bool,
        /// Permit pinned HTTPS hosts that resolve through a private local network route.
        #[arg(long)]
        allow_private_network: bool,
    },
}

/// `transcript` arguments.
#[derive(Debug, Args)]
pub struct TranscriptArgs {
    #[command(subcommand)]
    pub command: TranscriptCommand,
}

/// Existing-transcript operations.
#[derive(Debug, Subcommand)]
pub enum TranscriptCommand {
    /// Apply speaker display names and re-render Markdown without running ASR.
    Relabel {
        /// Existing `document-ir.json`.
        document_ir: PathBuf,
        /// `SPEAKER_ID=DISPLAY_NAME`; may be repeated.
        #[arg(long = "speaker", value_name = "ID=NAME", required = true)]
        speakers: Vec<String>,
        /// Markdown output path.
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
    },
}

/// `ui` local service arguments.
#[derive(Debug, Args)]
pub struct UiArgs {
    /// Loopback TCP port; zero asks the operating system for an available port.
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Do not open the service URL in the default browser.
    #[arg(long)]
    pub no_open: bool,

    /// Private local state directory for the service.
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

/// `formats` arguments.
#[derive(Debug, Args)]
pub struct FormatsArgs {
    #[command(subcommand)]
    pub command: Option<FormatsCommand>,
    /// Filter by format family.
    #[arg(long)]
    pub family: Option<String>,
    /// Filter by implementation status.
    #[arg(long)]
    pub status: Option<String>,
    /// Emit stable JSON instead of a human table.
    #[arg(long)]
    pub json: bool,
}

/// `formats` operations.
#[derive(Debug, Subcommand)]
pub enum FormatsCommand {
    /// Show one format descriptor.
    Show {
        format: String,
        #[arg(long)]
        json: bool,
    },
    /// Resolve an input and show ordered format hypotheses without conversion.
    Detect(DetectArgs),
}

/// Format detection arguments.
#[derive(Debug, Args)]
pub struct DetectArgs {
    pub input: OsString,
    #[arg(short = 'f', long)]
    pub format: Option<String>,
    #[arg(long)]
    pub extension: Option<String>,
    #[arg(long)]
    pub mime_type: Option<String>,
    #[arg(long)]
    pub charset: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub allow_network: bool,
    #[arg(long, requires = "allow_network")]
    pub allow_private_network: bool,
    #[arg(long, requires = "allow_network")]
    pub allow_host: Vec<String>,
}

/// `models` arguments.
#[derive(Debug, Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: Option<ModelsCommand>,
    #[arg(long)]
    pub json: bool,
}

/// Model management operations.
#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Show one model bundle.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Download and atomically install a hash-pinned model bundle.
    Install {
        id: String,
        #[arg(long)]
        insecure: bool,
        /// Permit the pinned HTTPS host to resolve through a private local network route.
        #[arg(long)]
        allow_private_network: bool,
    },
    /// Verify installed model files without networking.
    Verify {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a writable installed bundle.
    Remove { id: String },
    /// Print the installed path of a model bundle.
    Path { id: String },
}

/// `providers` arguments.
#[derive(Debug, Args)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: Option<ProvidersCommand>,
    #[arg(long)]
    pub json: bool,
}

/// Provider configuration operations.
#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// Show one configured provider.
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Add or replace an OpenAI-compatible provider configuration.
    Add(ProviderAddArgs),
    /// Remove a provider from one configuration scope.
    Remove {
        name: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Select the default provider in one configuration scope.
    SetDefault {
        name: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Show configured provider capabilities.
    Capabilities {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Perform a minimal provider connectivity check without document content.
    Test(ProviderTestArgs),
}

/// Add-provider arguments.
#[derive(Debug, Args)]
pub struct ProviderAddArgs {
    pub name: String,
    #[arg(long = "type", value_enum, default_value_t)]
    pub provider_type: ProviderType,
    #[arg(long)]
    pub base_url: String,
    #[arg(long)]
    pub model: String,
    #[arg(long)]
    pub api_key_env: String,
    #[arg(long)]
    pub capability: Vec<String>,
    #[arg(long, value_parser = parse_duration_ms)]
    pub timeout: Option<u64>,
    #[arg(long, value_enum, default_value_t)]
    pub scope: Scope,
}

/// Provider test arguments.
#[derive(Debug, Args)]
pub struct ProviderTestArgs {
    pub name: String,
    /// Authorize the connectivity check for this invocation.
    #[arg(long)]
    pub allow_network: bool,
    /// Additionally authorize loopback and private-network provider targets.
    #[arg(long, requires = "allow_network")]
    pub allow_private_network: bool,
    /// Further restrict the effective configured hostname allowlist.
    #[arg(long, requires = "allow_network")]
    pub allow_host: Vec<String>,
}

/// `plugins` arguments.
#[derive(Debug, Args)]
pub struct PluginsArgs {
    #[command(subcommand)]
    pub command: Option<PluginsCommand>,
    #[arg(long)]
    pub json: bool,
}

/// Plugin management operations.
#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Show one configured plugin.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate and install a local or hash-pinned HTTPS package.
    Install {
        source: String,
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Verify configured plugin packages.
    Verify {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Enable a configured plugin.
    Enable {
        id: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Disable a configured plugin.
    Disable {
        id: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Remove a configured plugin.
    Remove {
        id: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
}

/// `config` arguments.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

/// Configuration operations.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show automatic and explicitly loaded configuration paths.
    Paths {
        #[arg(long)]
        json: bool,
    },
    /// Show merged or fully resolved configuration with secrets redacted.
    Show {
        #[arg(long)]
        resolved: bool,
        #[arg(long, value_enum, default_value_t)]
        format: ConfigOutputFormat,
    },
    /// Create a documented configuration file.
    Init {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        force: bool,
    },
    /// Validate a file, or the complete discovered configuration when omitted.
    Validate { path: Option<PathBuf> },
    /// Read a dotted key from merged configuration.
    Get { key: String },
    /// Set a typed TOML value at a dotted key.
    Set {
        key: String,
        value: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Remove a dotted key.
    Unset {
        key: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Manage named configuration profiles.
    Profile(ProfileArgs),
}

/// Profile operations.
#[derive(Debug, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub command: ProfileCommand,
}

/// Profile management operations.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// List known profiles across loaded configuration layers.
    List,
    /// Create a profile, optionally copying another resolved profile.
    Create {
        name: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Remove a profile from one scope.
    Remove {
        name: String,
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
}

/// `doctor` arguments.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub allow_network: bool,
    #[arg(long, requires = "allow_network")]
    pub allow_private_network: bool,
}

/// Completion generation arguments.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

/// Version command arguments.
#[derive(Debug, Args)]
pub struct VersionArgs {
    #[arg(long)]
    pub json: bool,
}

/// Human-facing language.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Language {
    #[default]
    #[value(name = "en")]
    En,
    #[value(name = "zh-CN")]
    ZhCn,
}

/// Conditional terminal behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum When {
    #[default]
    Auto,
    Always,
    Never,
}

/// Diagnostic output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// Primary output artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum EmitKind {
    #[default]
    Markdown,
    #[value(name = "ir-json")]
    IrJson,
    #[value(name = "result-json")]
    ResultJson,
    Bundle,
}

impl EmitKind {
    /// Conventional filename extension for this artifact.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::IrJson | Self::ResultJson => "json",
            Self::Bundle => "mdpkg.zip",
        }
    }
}

/// CLI asset output mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum AssetModeArg {
    #[default]
    Extract,
    Embed,
    Omit,
}

/// Existing-output policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ConflictPolicy {
    #[default]
    Rename,
    Error,
    Overwrite,
}

/// Local OCR policy argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OcrPolicyArg {
    Off,
    Auto,
    Always,
}

/// Invalid-byte handling for plain-text decoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum EncodingErrorsArg {
    #[default]
    Strict,
    Replace,
}

/// CSV/TSV header-row selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum TableHeaderArg {
    #[default]
    Auto,
    Always,
    Never,
}

/// CSV/TSV ragged-record handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RaggedRowsArg {
    #[default]
    Strict,
    Pad,
}

/// Configuration mutation scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    #[default]
    Global,
    Project,
}

/// Provider adapter type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ProviderType {
    #[default]
    #[value(name = "openai-compatible")]
    OpenAiCompatible,
}

/// Configuration rendering format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ConfigOutputFormat {
    #[default]
    Toml,
    Json,
}

/// Supported completion shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}

/// Parse a binary byte size with an optional IEC suffix.
pub fn parse_byte_size(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    let suffixes = [("gib", 1024_u64.pow(3)), ("mib", 1024_u64.pow(2)), ("kib", 1024)];
    let (number, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, multiplier)| {
            lowercase.strip_suffix(suffix).map(|number| (number.trim(), *multiplier))
        })
        .unwrap_or((trimmed, 1));
    let base = number.parse::<u64>().map_err(|_| format!("invalid byte size '{value}'"))?;
    base.checked_mul(multiplier).ok_or_else(|| format!("byte size '{value}' is too large"))
}

/// Parse a duration into milliseconds.
pub fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    let suffixes = [("ms", 1_u64), ("s", 1_000), ("m", 60_000)];
    let (number, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, multiplier)| {
            lowercase.strip_suffix(suffix).map(|number| (number.trim(), *multiplier))
        })
        .unwrap_or((trimmed, 1));
    let base = number.parse::<u64>().map_err(|_| format!("invalid duration '{value}'"))?;
    if base == 0 {
        return Err("duration must be greater than zero".into());
    }
    base.checked_mul(multiplier).ok_or_else(|| format!("duration '{value}' is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iec_sizes_and_durations() {
        assert_eq!(parse_byte_size("2 MiB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_duration_ms("3s").unwrap(), 3_000);
        assert!(parse_duration_ms("0").is_err());
        assert!(parse_byte_size("many").is_err());
    }

    #[test]
    fn ui_is_a_command_but_double_dash_preserves_a_same_named_input() {
        let command = Cli::try_parse_from(["into-md", "ui", "--port", "0", "--no-open"]).unwrap();
        assert!(matches!(command.command, Some(Command::Ui(_))));

        let input = Cli::try_parse_from(["into-md", "--", "ui"]).unwrap();
        assert!(input.command.is_none());
        assert_eq!(input.conversion.inputs, [OsString::from("ui")]);
    }

    #[test]
    fn meeting_diarization_flags_are_bounded_and_require_diarization() {
        let command = Cli::try_parse_from([
            "into-md",
            "meeting.mp3",
            "--diarize",
            "--expected-speakers",
            "12",
        ])
        .unwrap();
        assert!(command.conversion.diarize);
        assert_eq!(command.conversion.expected_speakers, Some(12));
        assert!(
            Cli::try_parse_from(["into-md", "meeting.mp3", "--expected-speakers", "2",]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "into-md",
                "meeting.mp3",
                "--diarize",
                "--expected-speakers",
                "65",
            ])
            .is_err()
        );
    }

    #[test]
    fn media_setup_private_route_requires_an_explicit_flag() {
        let command =
            Cli::try_parse_from(["into-md", "setup", "media", "--allow-private-network"]).unwrap();
        assert!(matches!(
            command.command,
            Some(Command::Setup(SetupArgs {
                command: SetupCommand::Media { insecure: false, allow_private_network: true },
            }))
        ));
    }
}
