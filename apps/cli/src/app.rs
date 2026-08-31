//! CLI orchestration, input expansion, policy application, and management commands.

mod limits;
use limits::apply_limit_overrides;
#[path = "asset_paths.rs"]
mod asset_paths;
use asset_paths::*;

use crate::args::{
    AssetModeArg, CapabilitiesCommand, Cli, Command, CompletionShell, ConfigCommand,
    ConfigOutputFormat, ConflictPolicy, ConversionArgs, DetectArgs, EmitKind, EncodingErrorsArg,
    ErrorPolicyArg, FormatsCommand, LogFormat, OcrPolicyArg, PluginsCommand, ProfileCommand,
    ProviderType, ProvidersCommand, RaggedRowsArg, Scope, SetupCommand, TableHeaderArg,
    TranscriptCommand, UiArgs,
};
use crate::config::{self, LoadedConfig, PluginConfig, ProviderConfig};
use crate::error::{CliError, ExitClass};
use crate::i18n::{self, Catalog};
use crate::output::{
    self, BatchItemOutcome, BatchItemReport, BatchItemStatus, BatchLimitDto, BatchReport,
};
use clap::{CommandFactory, Parser};
use globset::{Glob, GlobSet, GlobSetBuilder};
use into_markdown::{
    AiMode, ArtifactSink, AssetMode, ConversionOptions, ConversionRequest, ConversionSummary,
    DetectionRequest, ErrorPolicy, FormatHint, InputFormat, InputRef, OcrPolicy,
    OpenAiCompatibleClient, ProviderConfig as TransportProviderConfig, ProviderNetworkPolicy,
    RaggedRowsMode, TableHeaderMode, TextDecodingMode,
};
use into_markdown_http_transport::{NetworkPolicy, TransportError, TransportErrorKind};
use into_markdown_plugin_manager::{ManagerError, ManagerErrorCode, PluginManager};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

// Self-contained OCR and speech capability packages intentionally
// include their audited models/runtimes. Keep the wire bound finite while
// allowing the reviewed packages plus release metadata.
const MAX_PLUGIN_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
// A remote upgrade can simultaneously retain the bounded download, the old
// rollback package, the authenticated incoming package, and the new extracted
// tree plus retained package. Keep that complete crash-safe peak within one
// explicit lifecycle budget instead of making large self-contained plugins
// installable but impossible to upgrade or repair.
const MAX_PLUGIN_LIFECYCLE_TEMPORARY_BYTES: u64 = 11 * 1024 * 1024 * 1024;
const PLUGIN_CLI_TRANSACTION: &str = ".cli-plugin-transaction.json";
const PLUGIN_CLI_TRANSACTION_NEXT: &str = ".cli-plugin-transaction.next";
const PLUGIN_CLI_TRANSACTION_PREVIOUS: &str = ".cli-plugin-transaction.previous";
const PLUGIN_CLI_LOCK: &str = ".cli-plugin.lock";
static PROCESS_SNAPSHOTS: OnceLock<
    Mutex<BTreeMap<String, into_markdown_plugin_manager::PreparedProcessPlugin>>,
> = OnceLock::new();

/// Drop every request-scoped immutable plugin snapshot before the CLI process exits.
///
/// `OnceLock` values are not destroyed during normal process teardown. Leaving the prepared
/// plugins in this static cache would therefore strand one complete speech runtime in the
/// operating-system temporary directory after every CLI invocation. The Web service still keeps
/// the cache for its full lifetime; the binary calls this only after [`run`] has returned.
pub(crate) fn release_process_snapshots() {
    let Some(cache) = PROCESS_SNAPSHOTS.get() else {
        return;
    };
    match cache.lock() {
        Ok(mut snapshots) => snapshots.clear(),
        Err(poisoned) => poisoned.into_inner().clear(),
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum CliPluginOperation {
    Install,
    Remove,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum CliPluginPhase {
    Started,
    StoreChanged,
    ConfigChanged,
    TrustChanged,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CliPluginTransaction {
    schema_version: u32,
    operation: CliPluginOperation,
    phase: CliPluginPhase,
    global: bool,
    store_relative: String,
    project_root: Option<PathBuf>,
    id: String,
    backup_name: Option<String>,
    old_config: Option<PluginConfig>,
    new_config: Option<PluginConfig>,
    signing_key_id: Option<String>,
    signing_key_sha256: Option<String>,
}

/// Process services supplied by the binary or tests.
pub struct RunContext<'a> {
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    pub stdin_is_terminal: bool,
    pub cwd: PathBuf,
    #[cfg(test)]
    pub user_data_anchor: Option<PathBuf>,
}

pub(crate) fn run_admin_cli_arguments(
    cwd: &Path,
    arguments: Vec<OsString>,
    test_user_data_anchor: Option<&Path>,
) -> Result<String, CliError> {
    let _ = test_user_data_anchor;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run(
        arguments,
        RunContext {
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal: true,
            cwd: cwd.to_owned(),
            #[cfg(test)]
            user_data_anchor: test_user_data_anchor.map(Path::to_owned),
        },
    )?;
    String::from_utf8(stdout).map_err(|_| CliError::internal("command returned non-UTF-8 output"))
}

pub(crate) const fn admin_plugin_target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[allow(unreachable_code)]
    "unsupported"
}

pub(crate) fn with_admin_authority<T>(
    test_user_data_anchor: Option<&Path>,
    operation: impl FnOnce() -> T,
) -> T {
    #[cfg(test)]
    let _test_user_data = TestUserDataGuard::set(test_user_data_anchor.map(Path::to_owned));
    #[cfg(test)]
    let _test_global_config = config::TestGlobalConfigGuard::set(
        test_user_data_anchor.map(|anchor| anchor.join("config/config.toml")),
    );
    let _ = test_user_data_anchor;
    operation()
}

#[cfg(test)]
thread_local! {
    static TEST_USER_DATA_ANCHOR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct TestUserDataGuard(Option<PathBuf>);

#[cfg(test)]
impl TestUserDataGuard {
    fn set(value: Option<PathBuf>) -> Self {
        let previous = TEST_USER_DATA_ANCHOR.with(|slot| slot.replace(value));
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for TestUserDataGuard {
    fn drop(&mut self) {
        TEST_USER_DATA_ANCHOR.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

/// Parse and execute one CLI invocation.
pub fn run(arguments: Vec<OsString>, mut context: RunContext<'_>) -> Result<(), CliError> {
    #[cfg(test)]
    let _test_user_data = TestUserDataGuard::set(context.user_data_anchor.clone());
    #[cfg(test)]
    let _test_global_config = config::TestGlobalConfigGuard::set(
        context.user_data_anchor.as_ref().map(|anchor| anchor.join("config/config.toml")),
    );
    let requested_language = i18n::requested_language(&arguments);
    if let Some(help) = i18n::localized_help(&arguments, requested_language) {
        context.stdout.write_all(help.as_bytes())?;
        return Ok(());
    }
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(OsString::from("into-md"));
    argv.extend(arguments);
    let mut cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) if error.use_stderr() => return Err(CliError::usage(error.to_string())),
        Err(error) => {
            context.stdout.write_all(error.to_string().as_bytes())?;
            return Ok(());
        }
    };
    if cli.command.is_none() && cli.conversion.inputs.is_empty() {
        if context.stdin_is_terminal {
            let mut command = Cli::command();
            command.write_long_help(&mut context.stdout)?;
            writeln!(context.stdout)?;
            return Ok(());
        }
        cli.conversion.inputs.push(OsString::from("-"));
    }

    // Plugin operations may have crashed after moving the configuration file
    // aside. Recover both the config-file transaction and the joint
    // store/config/trust transaction before the first configuration read, so
    // even read-only list/show/verify/run observes one coherent generation.
    if matches!(cli.command, Some(Command::Plugins(_) | Command::Doctor(_))) {
        recover_plugins_before_config_load(&context.cwd, cli.global.no_config)?;
    }

    let loaded = config::load(
        &context.cwd,
        &cli.global.config,
        cli.global.no_config,
        cli.global.profile.as_deref(),
        cli.global.language,
    )?;
    let catalog = Catalog::new(loaded.language);
    let language = loaded.language;
    let json_log = cli.global.log_format == Some(LogFormat::Json)
        || (cli.global.log_format.is_none()
            && loaded.effective.cli.log_format.as_deref() == Some("json"));
    if loaded.legacy_model_configuration && !cli.global.quiet {
        write_stderr_event(
            context.stderr,
            json_log,
            "warning",
            "legacyModelConfigurationIgnored",
            "legacy model_bundle configuration is ignored; install and select the corresponding capability plugin",
            None,
            "into-md: legacy model_bundle configuration is ignored; install and select the corresponding capability plugin",
        )?;
    }
    let result = match cli.command {
        None => {
            run_conversion(cli.conversion, &cli.global, loaded, catalog, json_log, &mut context)
        }
        Some(command) => run_command(command, &cli.global, loaded, catalog, json_log, &mut context),
    };
    result.map_err(|error| error.with_rendering(language, json_log))
}

pub(crate) fn recover_plugins_before_config_load(
    cwd: &Path,
    no_config: bool,
) -> Result<(), CliError> {
    if no_config {
        return Ok(());
    }
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let mut global_manager = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
    let execution = plugin_lifecycle_execution_context();
    // Acquiring either scope lock first performs its config-journal recovery.
    // The pending joint journal identifies which exact scope must then be
    // reconciled; `cwd` remains relevant for project identity validation.
    if config::scope_has_pending_transaction(crate::args::Scope::Global, cwd)? {
        drop(config::lock_scope(crate::args::Scope::Global, cwd)?);
    }
    if config::scope_has_pending_transaction(crate::args::Scope::Project, cwd)? {
        drop(config::lock_scope(crate::args::Scope::Project, cwd)?);
    }
    recover_pending_cli_plugin_transaction(
        &global_anchor,
        &global_relative,
        &mut global_manager,
        &execution,
    )?;
    let (project_anchor, project_relative) = project_plugin_store_scope(cwd)?;
    let project_manager = PluginManager::open_existing_scoped(
        &project_anchor,
        &project_relative,
        global_manager.trusted_signers(),
    )
    .map_err(plugin_manager_error)?;
    if let Some(project_manager) = project_manager {
        recover_pending_cli_plugin_transaction_at(
            project_manager.root(),
            &global_anchor,
            &global_relative,
            &mut global_manager,
            &execution,
        )?;
    }
    Ok(())
}

fn run_command(
    command: Command,
    global: &crate::args::GlobalArgs,
    loaded: LoadedConfig,
    catalog: Catalog,
    json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    match command {
        Command::Ui(arguments) => run_ui(arguments, global, loaded, context),
        Command::Formats(arguments) => match arguments.command {
            None => list_formats(
                arguments.family.as_deref(),
                arguments.status.as_deref(),
                arguments.json,
                context.stdout,
            ),
            Some(FormatsCommand::Show { format, json }) => {
                show_format(&format, json, context.stdout)
            }
            Some(FormatsCommand::Detect(arguments)) => {
                detect_format(arguments, loaded, context.stdout)
            }
        },
        Command::Capabilities(arguments) => {
            run_capabilities(arguments.command, arguments.json, &loaded, context)
        }
        Command::Setup(arguments) => {
            run_setup(arguments.command, global, &loaded, catalog, context)
        }
        Command::Transcript(arguments) => run_transcript(arguments.command, &loaded, context),
        Command::Providers(arguments) => {
            run_providers(arguments.command, arguments.json, &loaded, catalog, context)
        }
        Command::Plugins(arguments) => run_plugins(
            arguments.command,
            arguments.json,
            global.no_config,
            &loaded,
            catalog,
            context,
        ),
        Command::Config(arguments) => run_config(arguments.command, &loaded, context),
        Command::Doctor(arguments) => run_doctor(&arguments, &loaded, context),
        Command::Completions(arguments) => generate_completions(arguments.shell, context.stdout),
        Command::Version(arguments) => show_version(arguments.json, context.stdout),
    }?;
    if global.verbose > 1 && !global.quiet {
        write_stderr_event(
            context.stderr,
            json_log,
            "info",
            "configurationResolved",
            "configuration layers resolved successfully",
            None,
            "into-md: configuration layers resolved successfully",
        )?;
    }
    Ok(())
}

fn run_setup(
    command: SetupCommand,
    global: &crate::args::GlobalArgs,
    loaded: &LoadedConfig,
    catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    prepare_official_capability(command, global, loaded, catalog, context).map(drop)
}

pub(crate) fn prepare_official_capability(
    command: SetupCommand,
    global: &crate::args::GlobalArgs,
    loaded: &LoadedConfig,
    catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<LoadedConfig, CliError> {
    match command {
        SetupCommand::Ocr { insecure: _, allow_private_network: _ } => {
            if crate::embedded_runtime::enabled() {
                crate::services::verify_ocr_runtime(loaded, &context.cwd)?;
                writeln!(context.stdout, "OCR is built into into-md and is ready")?;
                return Ok(loaded.clone());
            }
            ensure_official_plugin("official.ocr.ppocrv6", global, loaded, catalog, context)?;
            let refreshed = reload_after_setup(global, context)?;
            crate::services::verify_ocr_runtime(&refreshed, &context.cwd)?;
            Ok(refreshed)
        }
        SetupCommand::Media { insecure: _, allow_private_network: _ } => {
            ensure_official_plugin("official.media.whisper", global, loaded, catalog, context)?;
            let refreshed = reload_after_setup(global, context)?;
            crate::services::verify_asr_runtime(&refreshed, &context.cwd)?;
            crate::services::verify_diarization_runtime(&refreshed, &context.cwd)?;
            Ok(refreshed)
        }
    }
}

fn reload_after_setup(
    global: &crate::args::GlobalArgs,
    context: &RunContext<'_>,
) -> Result<LoadedConfig, CliError> {
    config::load(
        &context.cwd,
        &global.config,
        global.no_config,
        global.profile.as_deref(),
        global.language,
    )
}

fn run_transcript(
    command: TranscriptCommand,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    match command {
        TranscriptCommand::Relabel { document_ir, speakers, output } => {
            let metadata = fs::metadata(&document_ir)?;
            if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
                return Err(CliError::usage(
                    "document IR must be a regular file no larger than 256 MiB",
                ));
            }
            let json = fs::read_to_string(&document_ir)?;
            let mut document = into_markdown::Document::from_json(&json)
                .map_err(|error| CliError::usage(format!("invalid document IR: {error}")))?;
            let mut present = std::collections::BTreeSet::new();
            collect_speaker_ids(&document.blocks, &mut present);
            let mut assigned = std::collections::BTreeSet::new();
            for assignment in speakers {
                let (speaker, name) = assignment
                    .split_once('=')
                    .ok_or_else(|| CliError::usage("--speaker must use SPEAKER_ID=DISPLAY_NAME"))?;
                if !valid_speaker_id(speaker) || !present.contains(speaker) {
                    return Err(CliError::usage(format!(
                        "speaker ID '{speaker}' is not present in this transcript"
                    )));
                }
                if !assigned.insert(speaker.to_owned()) {
                    return Err(CliError::usage(format!(
                        "speaker ID '{speaker}' was assigned more than once"
                    )));
                }
                if name.is_empty()
                    || name.trim() != name
                    || name.chars().count() > 80
                    || name.chars().any(char::is_control)
                {
                    return Err(CliError::usage(format!(
                        "display name for '{speaker}' is empty or invalid"
                    )));
                }
                document
                    .metadata
                    .properties
                    .insert(format!("media.speaker.{speaker}.label"), name.to_owned());
            }
            let markdown = into_markdown::render_markdown(
                &document,
                &[],
                &into_markdown::ConversionOptions::default(),
            )?;
            let execution = into_markdown::ExecutionContext::new(
                into_markdown::ExecutionOptions {
                    timeout: loaded.timeout_ms.map(std::time::Duration::from_millis),
                    ..into_markdown::ExecutionOptions::default()
                },
                loaded.options.limits.clone(),
            );
            output::write_file(&output, markdown.as_bytes(), ConflictPolicy::Error, &execution)?;
            writeln!(context.stdout, "{}", output.display())?;
            Ok(())
        }
    }
}

fn valid_speaker_id(value: &str) -> bool {
    value.strip_prefix("speaker-").is_some_and(|number| {
        !number.is_empty()
            && !number.starts_with('0')
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && number.parse::<u8>().is_ok_and(|value| (1..=64).contains(&value))
    })
}

fn collect_speaker_ids(
    blocks: &[into_markdown::BlockNode],
    output: &mut std::collections::BTreeSet<String>,
) {
    for node in blocks {
        match &node.block {
            into_markdown::Block::TimedSegment { speaker: Some(speaker), .. } => {
                output.insert(speaker.clone());
            }
            into_markdown::Block::List { items, .. } => {
                for item in items {
                    collect_speaker_ids(&item.blocks, output);
                }
            }
            into_markdown::Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        collect_speaker_ids(&cell.blocks, output);
                    }
                }
            }
            into_markdown::Block::Footnote { blocks, .. }
            | into_markdown::Block::Page { blocks, .. }
            | into_markdown::Block::Slide { blocks, .. }
            | into_markdown::Block::Sheet { blocks, .. } => collect_speaker_ids(blocks, output),
            _ => {}
        }
    }
}

fn run_ui(
    arguments: UiArgs,
    global: &crate::args::GlobalArgs,
    loaded: LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| CliError::component(format!("initialize local Web runtime: {error}")))?;
    runtime.block_on(crate::ui::run_cli(
        arguments,
        crate::admin::AdminConfigContext {
            explicit: global.config.clone(),
            no_automatic: global.no_config,
            profile: global.profile.clone(),
            language: global.language,
        },
        loaded,
        context.stdout,
        context.stderr,
    ))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FormatView<'a> {
    format: &'a str,
    family: &'a str,
    status: &'a str,
    source: &'a str,
    extensions: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_component: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_hint: Option<&'a str>,
}

fn list_formats(
    family: Option<&str>,
    status: Option<&str>,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let views = into_markdown::format_catalog()
        .iter()
        .filter(|entry| family.is_none_or(|family| entry.descriptor.family == family))
        .filter(|entry| status.is_none_or(|status| entry.descriptor.status.as_str() == status))
        .map(|entry| FormatView {
            format: entry.descriptor.format.as_str(),
            family: entry.descriptor.family,
            status: entry.descriptor.status.as_str(),
            source: entry.source.as_str(),
            extensions: entry.descriptor.extensions,
            runtime_component: entry.runtime.map(|runtime| runtime.component),
            install_hint: entry.runtime.map(|runtime| runtime.install_hint),
        })
        .collect::<Vec<_>>();
    if json {
        write_json(stdout, &views)
    } else {
        writeln!(stdout, "FORMAT\tFAMILY\tSTATUS\tSOURCE\tRUNTIME\tEXTENSIONS")?;
        for view in views {
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}\t{}",
                view.format,
                view.family,
                view.status,
                view.source,
                view.runtime_component.unwrap_or("-"),
                view.extensions.join(",")
            )?;
        }
        Ok(())
    }
}

fn show_format(value: &str, json: bool, stdout: &mut dyn Write) -> Result<(), CliError> {
    let entry =
        find_format(value).ok_or_else(|| CliError::usage(format!("unknown format '{value}'")))?;
    let descriptor = entry.descriptor;
    let view = FormatView {
        format: descriptor.format.as_str(),
        family: descriptor.family,
        status: descriptor.status.as_str(),
        source: entry.source.as_str(),
        extensions: descriptor.extensions,
        runtime_component: entry.runtime.map(|runtime| runtime.component),
        install_hint: entry.runtime.map(|runtime| runtime.install_hint),
    };
    if json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "format: {}", view.format)?;
        writeln!(stdout, "family: {}", view.family)?;
        writeln!(stdout, "status: {}", view.status)?;
        writeln!(stdout, "source: {}", view.source)?;
        if let Some(component) = view.runtime_component {
            writeln!(stdout, "runtime: {component}")?;
            writeln!(stdout, "install hint: {}", view.install_hint.unwrap_or_default())?;
        }
        writeln!(stdout, "extensions: {}", view.extensions.join(", "))?;
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectionView {
    source_name: Option<String>,
    source_size: u64,
    candidates: Vec<DetectionCandidateView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectionCandidateView {
    format: String,
    confidence: f32,
    explicit: bool,
    detector_id: String,
    reason: String,
    diagnostics: Vec<String>,
}

fn detect_format(
    arguments: DetectArgs,
    mut loaded: LoadedConfig,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    apply_network_authorization(
        &mut loaded.options,
        arguments.allow_network,
        arguments.allow_private_network,
        &arguments.allow_host,
    )?;
    let input = parse_input(&arguments.input)?;
    validate_input_network(&input, &loaded.options)?;
    let mut request = DetectionRequest::new(input);
    request.options = loaded.options;
    request.hint = FormatHint {
        format: arguments.format.as_deref().map(parse_format).transpose()?,
        extension: arguments.extension,
        media_type: arguments.mime_type,
        charset: arguments.charset,
        ..FormatHint::default()
    };
    let engine = into_markdown::default_engine().map_err(CliError::from)?;
    let result = futures::executor::block_on(engine.detect(request)).map_err(CliError::from)?;
    let view = DetectionView {
        source_name: result.source.name,
        source_size: result.source.size,
        candidates: result
            .candidates
            .into_iter()
            .map(|candidate| DetectionCandidateView {
                format: candidate.format.as_str().into(),
                confidence: candidate.confidence,
                explicit: candidate.explicit,
                detector_id: candidate.detector_id,
                reason: candidate.evidence,
                diagnostics: candidate.diagnostics,
            })
            .collect(),
    };
    if arguments.json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "FORMAT\tCONFIDENCE\tEXPLICIT\tDETECTOR\tREASON\tDIAGNOSTICS")?;
        for candidate in view.candidates {
            writeln!(
                stdout,
                "{}\t{:.3}\t{}\t{}\t{}\t{}",
                candidate.format,
                candidate.confidence,
                candidate.explicit,
                candidate.detector_id,
                candidate.reason,
                candidate.diagnostics.join("; ")
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapabilityView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub local_status: String,
    pub current_source: String,
    pub current_source_name: String,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at_ms: Option<u64>,
}

pub(crate) struct CapabilityInspection {
    pub capabilities: Vec<CapabilityView>,
    pub fingerprint: String,
}

const CORE_OCR_SOURCE: &str = "core:ocr";
const INTERNAL_OCR_SOURCE: &str = "plugin:official.ocr.ppocrv6/ocr";

fn run_capabilities(
    command: Option<CapabilitiesCommand>,
    json: bool,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    match command {
        None | Some(CapabilitiesCommand::List { json: false }) => {
            let entries = capability_views(loaded, &context.cwd)?;
            if json {
                write_json(
                    context.stdout,
                    &serde_json::json!({
                        "schemaVersion": 1,
                        "capabilities": entries,
                    }),
                )
            } else {
                writeln!(context.stdout, "CAPABILITY\tSTATUS\tSOURCE\tVERSION")?;
                for entry in entries {
                    writeln!(
                        context.stdout,
                        "{}\t{}\t{}\t{}",
                        entry.name,
                        entry.status,
                        entry.current_source_name,
                        entry.version.as_deref().unwrap_or("-")
                    )?;
                }
                Ok(())
            }
        }
        Some(CapabilitiesCommand::List { json: true }) => {
            let entries = capability_views(loaded, &context.cwd)?;
            write_json(
                context.stdout,
                &serde_json::json!({
                    "schemaVersion": 1,
                    "capabilities": entries,
                }),
            )
        }
        Some(CapabilitiesCommand::Show { id, json }) => {
            let entry = capability_views(loaded, &context.cwd)?
                .into_iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| CliError::usage(format!("unknown capability '{id}'")))?;
            if json {
                write_json(
                    context.stdout,
                    &serde_json::json!({"schemaVersion": 1, "capability": entry}),
                )
            } else {
                writeln!(context.stdout, "capability: {}", entry.name)?;
                writeln!(context.stdout, "status: {}", entry.status)?;
                writeln!(context.stdout, "source: {}", entry.current_source_name)?;
                writeln!(
                    context.stdout,
                    "available sources: {}",
                    entry
                        .sources
                        .iter()
                        .map(|source| capability_source_name(source))
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
                if let Some(version) = entry.version {
                    writeln!(context.stdout, "version: {version}")?;
                }
                Ok(())
            }
        }
        Some(CapabilitiesCommand::Verify { id, json }) => {
            run_capability_verify(&id, json, loaded, context)
        }
        Some(CapabilitiesCommand::Use { id, source, scope }) => {
            let config_source = capability_config_source(&id, &source, loaded)?;
            let path = config::set_capability_source(scope, &context.cwd, &id, &config_source)?;
            writeln!(context.stdout, "{}\t{}\t{}", id, source, path.display())?;
            Ok(())
        }
        Some(CapabilitiesCommand::Reset { id, scope }) => {
            let path = config::reset_capability_source(scope, &context.cwd, &id)?;
            writeln!(context.stdout, "{}\tdefault\t{}", id, path.display())?;
            Ok(())
        }
    }
}

fn run_capability_verify(
    id: &str,
    json: bool,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    if uses_embedded_ocr_verification(id, crate::embedded_runtime::enabled()) {
        let started = std::time::Instant::now();
        verify_capability_runtime(id, loaded, &context.cwd)?;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if json {
            return write_json(context.stdout, &core_ocr_verification_json(elapsed_ms));
        }
        writeln!(
            context.stdout,
            "{}: verified {} {} in {} ms",
            capability_name(id),
            capability_source_name(CORE_OCR_SOURCE),
            env!("CARGO_PKG_VERSION"),
            elapsed_ms
        )?;
        return Ok(());
    }
    let (plugin_id, shared) = capability_plugin(id)?;
    let started = std::time::Instant::now();
    let installed = verify_admin_effective_plugin_from_loaded(loaded, &context.cwd, plugin_id)?;
    verify_capability_runtime(id, loaded, &context.cwd)?;
    #[cfg(test)]
    let evidence_anchor = context.user_data_anchor.as_deref();
    #[cfg(not(test))]
    let evidence_anchor = None;
    crate::ui::record_capability_verification(loaded, &context.cwd, plugin_id, evidence_anchor)?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if json {
        return write_json(
            context.stdout,
            &serde_json::json!({
                "schemaVersion": 1,
                "capability": id,
                "plugin": plugin_id,
                "pluginName": capability_plugin_name(plugin_id),
                "status": "ready",
                "version": installed.version,
                "elapsedMs": elapsed_ms,
                "sharedCapabilities": shared,
            }),
        );
    }
    writeln!(
        context.stdout,
        "{}: verified {} {} in {} ms",
        capability_name(id),
        capability_plugin_name(plugin_id),
        installed.version,
        elapsed_ms
    )?;
    if shared.len() > 1 {
        writeln!(
            context.stdout,
            "shared verification: {}",
            shared.iter().map(|id| capability_name(id)).collect::<Vec<_>>().join(", ")
        )?;
    }
    Ok(())
}

fn uses_embedded_ocr_verification(id: &str, embedded_ocr: bool) -> bool {
    embedded_ocr && id == "ocr"
}

fn core_ocr_verification_json(elapsed_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "capability": "ocr",
        "source": CORE_OCR_SOURCE,
        "sourceName": capability_source_name(CORE_OCR_SOURCE),
        "status": "ready",
        "version": env!("CARGO_PKG_VERSION"),
        "elapsedMs": elapsed_ms,
        "sharedCapabilities": ["ocr"],
    })
}

pub(crate) fn capability_views(
    loaded: &LoadedConfig,
    cwd: &Path,
) -> Result<Vec<CapabilityView>, CliError> {
    inspect_capabilities(loaded, cwd).map(|inspection| inspection.capabilities)
}

pub(crate) fn inspect_capabilities(
    loaded: &LoadedConfig,
    cwd: &Path,
) -> Result<CapabilityInspection, CliError> {
    const ITEMS: [(&str, &str, &str); 3] = [
        ("ocr", "official.ocr.ppocrv6", "ocr"),
        ("transcription", "official.media.whisper", "transcription"),
        ("diarization", "official.media.whisper", "diarization"),
    ];
    let mut inspected = std::collections::BTreeMap::new();
    let mut status_tokens = BTreeMap::new();
    for (_, plugin_id, _) in ITEMS {
        if inspected.contains_key(plugin_id) {
            continue;
        }
        if plugin_id == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
            inspected.insert(plugin_id, PluginInspection::BuiltIn);
            continue;
        }
        let configured = loaded.effective.plugins.get(plugin_id);
        let result = match configured {
            Some(plugin) if !plugin.enabled => PluginInspection::Disabled,
            Some(_) => {
                match inspect_admin_effective_plugin_status_from_loaded(loaded, cwd, plugin_id) {
                    Ok((installed, token)) => {
                        status_tokens.insert(plugin_id, token);
                        PluginInspection::Ready(installed)
                    }
                    Err(error) => PluginInspection::Invalid(error.code().to_owned()),
                }
            }
            None => PluginInspection::NotInstalled,
        };
        inspected.insert(plugin_id, result);
    }
    let capabilities = capability_views_from_inspections(loaded, &inspected)?;
    let fingerprint = capability_snapshot_fingerprint(loaded, &status_tokens)?;
    Ok(CapabilityInspection { capabilities, fingerprint })
}

pub(crate) fn checking_capability_views(loaded: &LoadedConfig) -> Vec<CapabilityView> {
    let inspected = [
        (
            "official.ocr.ppocrv6",
            if crate::embedded_runtime::enabled() {
                PluginInspection::BuiltIn
            } else {
                PluginInspection::Checking
            },
        ),
        ("official.media.whisper", PluginInspection::Checking),
    ]
    .into_iter()
    .collect();
    capability_views_from_inspections(loaded, &inspected).unwrap_or_default()
}

fn capability_snapshot_fingerprint(
    loaded: &LoadedConfig,
    status_tokens: &BTreeMap<&str, String>,
) -> Result<String, CliError> {
    let executable = std::env::current_exe().ok().and_then(|path| {
        let metadata = path.metadata().ok()?;
        Some(serde_json::json!({
            "bytes": metadata.len(),
            "modifiedMs": metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis(),
        }))
    });
    let bytes = serde_json::to_vec(&serde_json::json!({
        "coreVersion": env!("CARGO_PKG_VERSION"),
        "coreBuild": executable,
        "target": admin_plugin_target(),
        "plugins": loaded.effective.plugins,
        "providers": loaded.effective.providers,
        "defaultProvider": loaded.effective.default_provider,
        "routes": loaded.effective.capability_routes,
        "pluginStatusTokens": status_tokens,
    }))
    .map_err(|error| CliError::internal(format!("serialize capability snapshot key: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn capability_views_from_inspections(
    loaded: &LoadedConfig,
    inspected: &std::collections::BTreeMap<&str, PluginInspection>,
) -> Result<Vec<CapabilityView>, CliError> {
    const ITEMS: [(&str, &str, &str); 3] = [
        ("ocr", "official.ocr.ppocrv6", "ocr"),
        ("transcription", "official.media.whisper", "transcription"),
        ("diarization", "official.media.whisper", "diarization"),
    ];
    ITEMS
        .into_iter()
        .map(|(id, plugin_id, plugin_capability)| {
            let route = capability_route(loaded, id);
            let inspection = &inspected[plugin_id];
            let built_in_ocr = id == "ocr" && matches!(inspection, PluginInspection::BuiltIn);
            let internal_local = format!("plugin:{plugin_id}/{plugin_capability}");
            let local =
                if built_in_ocr { CORE_OCR_SOURCE.to_owned() } else { internal_local.clone() };
            let (local_status, version) = match inspection {
                PluginInspection::BuiltIn => ("ready", Some(env!("CARGO_PKG_VERSION").to_owned())),
                PluginInspection::Ready(installed) => ("ready", Some(installed.version.clone())),
                PluginInspection::Disabled => ("disabled", None),
                PluginInspection::Invalid(code) if code == "componentUnavailable" => {
                    ("incompatible", None)
                }
                PluginInspection::Invalid(code) if code == "notFound" => ("not-installed", None),
                PluginInspection::Invalid(_) => ("corrupt", None),
                PluginInspection::NotInstalled => ("not-installed", None),
                PluginInspection::Checking => ("checking", None),
            };
            let local_ready = local_status == "ready";
            // The official local source remains selectable while absent so the
            // management UI can offer installation without changing identity.
            let mut sources = vec![local.clone()];
            for (provider_id, provider) in &loaded.effective.providers {
                if let Some(capability_id) = provider_capability_id(&provider.capabilities, id) {
                    sources.push(format!("provider:{provider_id}/{capability_id}"));
                }
            }
            sources.push("off".into());
            let current_source = route.primary.clone().unwrap_or_else(|| {
                if local_status == "not-installed" { "off".into() } else { internal_local }
            });
            let current_source = if built_in_ocr && current_source == INTERNAL_OCR_SOURCE {
                CORE_OCR_SOURCE.to_owned()
            } else {
                current_source
            };
            let remote_ready = current_source
                .strip_prefix("provider:")
                .and_then(|value| value.split_once('/'))
                .and_then(|(provider_id, _)| loaded.effective.providers.get(provider_id))
                .is_some_and(|provider| std::env::var_os(&provider.api_key_env).is_some());
            let status = match current_source.as_str() {
                "off" if local_ready => "disabled",
                "off" => "not-installed",
                CORE_OCR_SOURCE if built_in_ocr => "ready",
                value if value.starts_with("plugin:") && local_ready => "ready",
                value if value.starts_with("plugin:") => local_status,
                value if value.starts_with("provider:") && remote_ready => "ready",
                value if value.starts_with("provider:") => "blocked",
                _ => "incompatible",
            };
            Ok(CapabilityView {
                id: id.into(),
                name: capability_name(id).into(),
                status: status.into(),
                local_status: local_status.into(),
                current_source_name: capability_source_name(&current_source),
                current_source,
                sources,
                local_version: version.clone(),
                version,
                last_verified_at_ms: None,
            })
        })
        .collect()
}

enum PluginInspection {
    BuiltIn,
    Ready(into_markdown_plugin_manager::InstalledPlugin),
    Disabled,
    Invalid(String),
    NotInstalled,
    Checking,
}

pub(crate) fn capability_name(id: &str) -> &'static str {
    match id {
        "ocr" => "图片 OCR",
        "transcription" => "语音转写",
        "diarization" => "说话人识别",
        _ => "未知能力",
    }
}

pub(crate) fn capability_plugin_name(id: &str) -> &'static str {
    match id {
        "official.ocr.ppocrv6" => "本地 OCR（PP-OCR）",
        "official.media.whisper" => "本地语音（Whisper）",
        _ => "本地扩展",
    }
}

fn capability_source_name(source: &str) -> String {
    if source == "off" {
        return "关闭".into();
    }
    if source == CORE_OCR_SOURCE {
        return "内置 OCR".into();
    }
    if let Some((plugin_id, _)) = source.strip_prefix("plugin:").and_then(|v| v.split_once('/')) {
        return capability_plugin_name(plugin_id).into();
    }
    if let Some((provider_id, _)) = source.strip_prefix("provider:").and_then(|v| v.split_once('/'))
    {
        return format!("AI 服务：{provider_id}");
    }
    source.into()
}

pub(crate) fn capability_plugin(
    id: &str,
) -> Result<(&'static str, &'static [&'static str]), CliError> {
    match id {
        "ocr" => Ok(("official.ocr.ppocrv6", &["ocr"])),
        "transcription" | "diarization" => {
            Ok(("official.media.whisper", &["transcription", "diarization"]))
        }
        _ => Err(CliError::usage(format!("unknown capability '{id}'"))),
    }
}

pub(crate) fn verify_capability_runtime(
    id: &str,
    loaded: &LoadedConfig,
    cwd: &Path,
) -> Result<(), CliError> {
    let result = match id {
        "ocr" => crate::services::verify_ocr_runtime(loaded, cwd),
        "transcription" => crate::services::verify_asr_runtime(loaded, cwd),
        "diarization" => crate::services::verify_diarization_runtime(loaded, cwd),
        _ => return Err(CliError::usage(format!("unknown capability '{id}'"))),
    };
    result.map_err(CliError::from)
}

fn capability_route<'a>(loaded: &'a LoadedConfig, id: &str) -> &'a config::CapabilityRouteConfig {
    match id {
        "ocr" => &loaded.effective.capability_routes.ocr,
        "transcription" => &loaded.effective.capability_routes.transcription,
        "diarization" => &loaded.effective.capability_routes.diarization,
        _ => unreachable!("validated product capability"),
    }
}

fn provider_supports_capability(capabilities: &[String], id: &str) -> bool {
    provider_capability_id(capabilities, id).is_some()
}

fn provider_capability_id<'a>(capabilities: &'a [String], id: &str) -> Option<&'a str> {
    let preferred: &[&str] = match id {
        "ocr" => &["vision-ocr", "ocr"],
        "transcription" => &["audio-transcription", "transcription"],
        // No remote diarization adapter is published yet. A Provider's raw
        // capability declaration must not create a selectable dead route.
        _ => return None,
    };
    preferred
        .iter()
        .find_map(|candidate| capabilities.iter().find(|value| value.as_str() == *candidate))
        .map(String::as_str)
}

fn validate_capability_source(
    id: &str,
    source: &str,
    loaded: &LoadedConfig,
) -> Result<(), CliError> {
    use into_markdown_provider_plugin::{CapabilityId, CapabilitySourceRef};
    use std::str::FromStr as _;
    let capability = CapabilityId::from_str(id)
        .map_err(|_| CliError::usage(format!("unknown capability '{id}'")))?;
    if source == CORE_OCR_SOURCE {
        return if capability == CapabilityId::Ocr && crate::embedded_runtime::enabled() {
            Ok(())
        } else {
            Err(CliError::usage(format!("source '{source}' cannot provide '{id}'")))
        };
    }
    let parsed = CapabilitySourceRef::from_str(source)
        .map_err(|_| CliError::usage(format!("invalid capability source '{source}'")))?;
    match parsed {
        CapabilitySourceRef::Off => Ok(()),
        CapabilitySourceRef::Plugin { plugin_id, capability_id } => {
            let expected = match capability {
                CapabilityId::LegacyOffice => {
                    return Err(CliError::usage(
                        "legacy Office is a built-in Core capability and has no selectable source",
                    ));
                }
                CapabilityId::Ocr => ("official.ocr.ppocrv6", "ocr"),
                CapabilityId::Transcription => ("official.media.whisper", "transcription"),
                CapabilityId::Diarization => ("official.media.whisper", "diarization"),
            };
            if (plugin_id.as_str(), capability_id.as_str()) != expected {
                return Err(CliError::usage(format!("source '{source}' cannot provide '{id}'")));
            }
            if !loaded.effective.plugins.get(&plugin_id).is_some_and(|plugin| plugin.enabled) {
                return Err(CliError::component(format!(
                    "plugin '{plugin_id}' is not installed and enabled"
                )));
            }
            Ok(())
        }
        CapabilitySourceRef::Provider { provider_id, capability_id } => {
            let provider = loaded
                .effective
                .providers
                .get(&provider_id)
                .ok_or_else(|| CliError::usage(format!("unknown provider '{provider_id}'")))?;
            if !provider_supports_capability(&provider.capabilities, id)
                || !matches!(capability_id.as_str(), value if value == id || id == "ocr" && value == "vision-ocr" || id == "transcription" && value == "audio-transcription")
            {
                return Err(CliError::usage(format!(
                    "provider '{provider_id}' does not provide '{id}'"
                )));
            }
            Ok(())
        }
    }
}

pub(crate) fn capability_config_source(
    id: &str,
    source: &str,
    loaded: &LoadedConfig,
) -> Result<String, CliError> {
    validate_capability_source(id, source, loaded)?;
    Ok(if id == "ocr" && source == CORE_OCR_SOURCE {
        INTERNAL_OCR_SOURCE.to_owned()
    } else {
        source.to_owned()
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderView<'a> {
    name: &'a str,
    provider_type: &'a str,
    base_url: String,
    model: &'a str,
    models: &'a std::collections::BTreeMap<String, String>,
    api_key_env: &'a str,
    capabilities: &'a [String],
    allowed_hosts: &'a [String],
    allow_private_network: bool,
    default: bool,
}

#[allow(clippy::too_many_lines)]
fn run_providers(
    command: Option<ProvidersCommand>,
    json: bool,
    loaded: &LoadedConfig,
    _catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    match command {
        None => list_providers(loaded, json, context.stdout),
        Some(ProvidersCommand::Show { name, json }) => {
            show_provider(loaded, &name, json, context.stdout)
        }
        Some(ProvidersCommand::Capabilities { name, json }) => {
            let provider = loaded
                .effective
                .providers
                .get(&name)
                .ok_or_else(|| CliError::usage(format!("unknown provider '{name}'")))?;
            if json {
                write_json(context.stdout, &provider.capabilities)
            } else {
                for capability in &provider.capabilities {
                    writeln!(context.stdout, "{capability}")?;
                }
                Ok(())
            }
        }
        Some(ProvidersCommand::Add(arguments)) => {
            config::validate_environment_name(&arguments.api_key_env)?;
            let allowed_hosts = config::normalize_allowed_hosts(&arguments.allow_host)?;
            for capability in &arguments.capability {
                config::validate_capability(capability)?;
            }
            let mut models = std::collections::BTreeMap::new();
            for mapping in &arguments.model_map {
                let (capability, model) = mapping
                    .split_once('=')
                    .ok_or_else(|| CliError::usage("--model-map must use CAPABILITY=MODEL"))?;
                config::validate_capability(capability)?;
                if model.is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
                    return Err(CliError::usage("--model-map model is invalid"));
                }
                if !arguments.capability.iter().any(|value| value == capability) {
                    return Err(CliError::usage(format!(
                        "--model-map capability '{capability}' must also be declared with --capability"
                    )));
                }
                if models.insert(capability.to_owned(), model.to_owned()).is_some() {
                    return Err(CliError::usage(format!(
                        "duplicate --model-map capability '{capability}'"
                    )));
                }
            }
            let parsed = url::Url::parse(&arguments.base_url)
                .map_err(|error| CliError::usage(format!("invalid provider URL: {error}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(CliError::usage("provider URL must use http or https"));
            }
            let provider = ProviderConfig {
                provider_type: match arguments.provider_type {
                    ProviderType::OpenAiCompatible => "openai-compatible".into(),
                },
                base_url: arguments.base_url,
                model: arguments.model,
                models,
                api_key_env: arguments.api_key_env,
                timeout_ms: arguments.timeout,
                capabilities: arguments.capability,
                allowed_hosts,
                allow_private_network: arguments.allow_private_network,
            };
            let path =
                config::add_provider(arguments.scope, &context.cwd, &arguments.name, &provider)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(ProvidersCommand::Remove { name, scope }) => {
            let path = config::remove_provider(scope, &context.cwd, &name)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(ProvidersCommand::SetDefault { name, scope }) => {
            let provider_exists = if scope == Scope::Global {
                crate::config::has_complete_provider_in_scope(scope, &context.cwd, &name)?
            } else {
                loaded.effective.providers.contains_key(&name)
            };
            if !provider_exists {
                return Err(CliError::usage(format!("unknown provider '{name}'")));
            }
            let path = config::set_default_provider(scope, &context.cwd, &name)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(ProvidersCommand::Test(arguments)) => {
            let result = test_provider(loaded, &arguments)?;
            if json {
                write_json(context.stdout, &result)
            } else {
                writeln!(context.stdout, "provider: {}", arguments.name)?;
                writeln!(context.stdout, "model available: {}", result.configured_model_available)?;
                writeln!(context.stdout, "models observed: {}", result.model_count)?;
                writeln!(context.stdout, "capabilities: {}", result.capabilities.join(", "))?;
                Ok(())
            }
        }
    }
}

pub(crate) fn test_provider(
    loaded: &LoadedConfig,
    arguments: &crate::args::ProviderTestArgs,
) -> Result<into_markdown::ProviderTestResult, CliError> {
    let provider = loaded
        .effective
        .providers
        .get(&arguments.name)
        .ok_or_else(|| CliError::usage(format!("unknown provider '{}'", arguments.name)))?;
    let mut options = loaded.options.clone();
    narrow_allowed_hosts(&mut options.network.allowed_hosts, &provider.allowed_hosts)?;
    apply_network_authorization(
        &mut options,
        arguments.allow_network,
        provider.allow_private_network && arguments.allow_private_network,
        &arguments.allow_host,
    )?;
    validate_network_url(&provider.base_url, &options, "provider")?;
    let timeout = std::time::Duration::from_millis(
        provider.timeout_ms.or(loaded.timeout_ms).unwrap_or(30_000),
    );
    let provider_config = TransportProviderConfig::parse(
        &provider.base_url,
        &provider.model,
        &provider.api_key_env,
        timeout,
        provider.capabilities.clone(),
    )?;
    let client = OpenAiCompatibleClient::new(
        provider_config,
        ProviderNetworkPolicy {
            allow_network: options.network.enabled,
            allow_private_network: !options.network.deny_private_networks,
            allowed_hosts: options.network.allowed_hosts.clone(),
        },
    );
    let execution = into_markdown::ExecutionContext::new(
        into_markdown::ExecutionOptions {
            timeout: Some(timeout),
            ..into_markdown::ExecutionOptions::default()
        },
        options.limits,
    );
    client.test(&execution).map_err(CliError::from)
}

fn list_providers(
    loaded: &LoadedConfig,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let views = loaded
        .effective
        .providers
        .iter()
        .map(|(name, provider)| provider_view(loaded, name, provider))
        .collect::<Vec<_>>();
    if json {
        write_json(stdout, &views)
    } else {
        writeln!(stdout, "PROVIDER\tTYPE\tMODEL\tCAPABILITIES\tDEFAULT")?;
        for view in views {
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}",
                view.name,
                view.provider_type,
                view.model,
                view.capabilities.join(","),
                view.default
            )?;
        }
        Ok(())
    }
}

fn show_provider(
    loaded: &LoadedConfig,
    name: &str,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let provider = loaded
        .effective
        .providers
        .get(name)
        .ok_or_else(|| CliError::usage(format!("unknown provider '{name}'")))?;
    let view = provider_view(loaded, name, provider);
    if json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "name: {}", view.name)?;
        writeln!(stdout, "type: {}", view.provider_type)?;
        writeln!(stdout, "base URL: {}", view.base_url)?;
        writeln!(stdout, "model: {}", view.model)?;
        writeln!(stdout, "API key environment: {}", view.api_key_env)?;
        writeln!(stdout, "capabilities: {}", view.capabilities.join(", "))?;
        writeln!(
            stdout,
            "allowed hosts: {}",
            if view.allowed_hosts.is_empty() {
                "-".to_owned()
            } else {
                view.allowed_hosts.join(", ")
            }
        )?;
        writeln!(stdout, "allow private network: {}", view.allow_private_network)?;
        Ok(())
    }
}

fn provider_view<'a>(
    loaded: &'a LoadedConfig,
    name: &'a str,
    provider: &'a ProviderConfig,
) -> ProviderView<'a> {
    ProviderView {
        name,
        provider_type: &provider.provider_type,
        base_url: redact_url(&provider.base_url),
        model: &provider.model,
        models: &provider.models,
        api_key_env: &provider.api_key_env,
        capabilities: &provider.capabilities,
        allowed_hosts: &provider.allowed_hosts,
        allow_private_network: provider.allow_private_network,
        default: loaded.effective.default_provider.as_deref() == Some(name),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OfficialPluginCatalog {
    schema_version: u32,
    signing_key_id: String,
    signing_key_sha256: String,
    packages: BTreeMap<String, OfficialPluginRecord>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OfficialPluginRecord {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    url: Option<String>,
    sha256: String,
}

#[derive(Debug)]
pub(crate) struct OfficialPackageAuthority {
    pub plugin_id: String,
    pub signing_key_id: String,
    pub signing_key_sha256: String,
}

fn installed_official_plugin_catalog(
    required_for: Option<&str>,
) -> Result<Option<(PathBuf, OfficialPluginCatalog)>, CliError> {
    if let Some(bytes) =
        crate::embedded_runtime::official_publisher_catalog().map_err(CliError::component)?
    {
        let catalog: OfficialPluginCatalog = serde_json::from_slice(&bytes)
            .map_err(|_| CliError::component("embedded official plugin catalog is invalid"))?;
        if !matches!(catalog.schema_version, 1 | 2) {
            return Err(CliError::component(
                "embedded official plugin catalog schema is unsupported",
            ));
        }
        config::validate_sha256(&catalog.signing_key_sha256)?;
        return Ok(Some((PathBuf::new(), catalog)));
    }
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|_| CliError::component("installed executable path is unavailable"))?;
    let distribution = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| CliError::component("installed distribution root is unavailable"))?;
    let plugin_root = distribution.join("share/into-markdown/plugins");
    let catalog_path = plugin_root.join("official-publisher.json");
    let metadata = match fs::symlink_metadata(&catalog_path) {
        Ok(metadata) => metadata,
        Err(_) if required_for.is_none() => return Ok(None),
        Err(_) => {
            return Err(CliError::component(format!(
                "official plugin catalog is unavailable for {}",
                required_for.unwrap_or("requested package")
            )));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return Err(CliError::component("official plugin catalog is invalid"));
    }
    let bytes = fs::read(&catalog_path).map_err(CliError::from)?;
    let catalog: OfficialPluginCatalog = serde_json::from_slice(&bytes)
        .map_err(|_| CliError::component("official plugin catalog is invalid"))?;
    if !matches!(catalog.schema_version, 1 | 2) {
        return Err(CliError::component("official plugin catalog schema is unsupported"));
    }
    config::validate_sha256(&catalog.signing_key_sha256)?;
    Ok(Some((plugin_root, catalog)))
}

pub(crate) fn official_package_authority_for_sha256(
    sha256: &str,
) -> Result<Option<OfficialPackageAuthority>, CliError> {
    config::validate_sha256(sha256)?;
    let Some((_, catalog)) = installed_official_plugin_catalog(None)? else {
        return Ok(None);
    };
    let mut matched = catalog.packages.iter().filter(|(_, package)| package.sha256 == sha256);
    let Some((plugin_id, package)) = matched.next() else {
        return Ok(None);
    };
    if matched.next().is_some() {
        return Err(CliError::component("official plugin catalog contains a duplicate digest"));
    }
    config::validate_sha256(&package.sha256)?;
    Ok(Some(OfficialPackageAuthority {
        plugin_id: plugin_id.clone(),
        signing_key_id: catalog.signing_key_id,
        signing_key_sha256: catalog.signing_key_sha256,
    }))
}

fn ensure_official_plugin(
    id: &str,
    global: &crate::args::GlobalArgs,
    loaded: &LoadedConfig,
    catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    if loaded.effective.plugins.get(id).is_some_and(|plugin| plugin.enabled)
        && verify_admin_effective_plugin_from_loaded(loaded, &context.cwd, id).is_ok()
    {
        return Ok(());
    }
    let (plugin_root, official) =
        installed_official_plugin_catalog(Some(id))?.ok_or_else(|| {
            CliError::component(format!("official plugin catalog is unavailable for {id}"))
        })?;
    let package = official
        .packages
        .get(id)
        .ok_or_else(|| CliError::component(format!("official package {id} is unavailable")))?;
    config::validate_sha256(&package.sha256)?;
    let source = match (&package.file, &package.url) {
        (Some(file), None) => {
            if !matches!(
                Path::new(file).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            ) {
                return Err(CliError::component("official package filename is invalid"));
            }
            plugin_root.join("packages").join(file).display().to_string()
        }
        (None, Some(url)) => url.clone(),
        _ => return Err(CliError::component("official package source is invalid")),
    };
    run_plugins(
        Some(PluginsCommand::Install {
            source,
            sha256: Some(package.sha256.clone()),
            signing_key_id: Some(official.signing_key_id),
            signing_key_sha256: Some(official.signing_key_sha256),
            scope: Scope::Global,
        }),
        false,
        global.no_config,
        loaded,
        catalog,
        context,
    )
}

fn plugin_lifecycle_execution_context() -> into_markdown::ExecutionContext {
    let limits = into_markdown::ResourceLimits {
        max_temporary_bytes: MAX_PLUGIN_LIFECYCLE_TEMPORARY_BYTES,
        ..into_markdown::ResourceLimits::default()
    };
    into_markdown::ExecutionContext::new(into_markdown::ExecutionOptions::default(), limits)
}

fn run_plugins(
    command: Option<PluginsCommand>,
    json: bool,
    no_config: bool,
    loaded: &LoadedConfig,
    _catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    if no_config && !matches!(&command, None | Some(PluginsCommand::Show { .. })) {
        return Err(CliError::usage("--no-config supports only plugin list and show operations"));
    }
    match command {
        None => {
            if json {
                let plugins = loaded
                    .effective
                    .plugins
                    .iter()
                    .map(|(id, plugin)| PluginView {
                        id,
                        source: redact_url(&plugin.source),
                        sha256: plugin.sha256.as_deref(),
                        protocol: &plugin.protocol,
                        enabled: plugin.enabled,
                    })
                    .collect::<Vec<_>>();
                write_json(context.stdout, &plugins)
            } else {
                writeln!(context.stdout, "PLUGIN\tPROTOCOL\tENABLED\tSOURCE")?;
                for (id, plugin) in &loaded.effective.plugins {
                    writeln!(
                        context.stdout,
                        "{}\t{}\t{}\t{}",
                        id,
                        plugin.protocol,
                        plugin.enabled,
                        redact_url(&plugin.source)
                    )?;
                }
                Ok(())
            }
        }
        Some(PluginsCommand::Show { id, json }) => {
            let plugin = loaded
                .effective
                .plugins
                .get(&id)
                .ok_or_else(|| CliError::usage(format!("unknown plugin '{id}'")))?;
            if json {
                write_json(
                    context.stdout,
                    &PluginView {
                        id: &id,
                        source: redact_url(&plugin.source),
                        sha256: plugin.sha256.as_deref(),
                        protocol: &plugin.protocol,
                        enabled: plugin.enabled,
                    },
                )
            } else {
                writeln!(context.stdout, "id: {id}")?;
                writeln!(context.stdout, "protocol: {}", plugin.protocol)?;
                writeln!(context.stdout, "enabled: {}", plugin.enabled)?;
                writeln!(context.stdout, "source: {}", redact_url(&plugin.source))?;
                Ok(())
            }
        }
        Some(PluginsCommand::Install {
            source,
            sha256,
            signing_key_id,
            signing_key_sha256,
            scope,
        }) => {
            if source.starts_with("https://") {
                let hash = sha256.as_deref().ok_or_else(|| {
                    CliError::usage("HTTPS plugin installation requires --sha256")
                })?;
                config::validate_sha256(hash)?;
            } else if source.starts_with("http://") {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "insecureSource",
                    "plugin URLs must use HTTPS",
                ));
            } else if !Path::new(&source).exists() {
                return Err(CliError::new(
                    ExitClass::Io,
                    "io",
                    format!("plugin package does not exist: {source}"),
                ));
            }
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let mut global_manager =
                PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                    .map_err(plugin_manager_error)?;
            let execution = plugin_lifecycle_execution_context();
            let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
            recover_pending_cli_plugin_transaction(
                &global_anchor,
                &global_relative,
                &mut global_manager,
                &execution,
            )?;
            match (signing_key_id.as_deref(), signing_key_sha256.as_deref()) {
                (Some(id), Some(fingerprint)) => {
                    config::validate_sha256(fingerprint)?;
                    if scope == crate::args::Scope::Project
                        && global_manager.trusted_signers().fingerprints.get(id).map(String::as_str)
                            != Some(fingerprint)
                    {
                        return Err(CliError::new(
                            ExitClass::Policy,
                            "untrustedPluginPublisher",
                            "project scope can only reference a publisher trusted globally",
                        ));
                    }
                }
                (None, None) => {}
                _ => return Err(CliError::usage("both signing key options are required")),
            }
            let global_candidate = match (signing_key_id.as_deref(), signing_key_sha256.as_deref())
            {
                (Some(id), Some(fingerprint)) if scope == crate::args::Scope::Global => {
                    global_manager
                        .with_candidate_signer(id, fingerprint)
                        .map_err(plugin_manager_error)?
                }
                _ => global_manager.clone(),
            };
            let project_scope = if scope == crate::args::Scope::Project {
                Some(ProjectScopeAuthority::resolve(&context.cwd)?)
            } else {
                None
            };
            let (store_relative, project_root, manager) = if scope == crate::args::Scope::Global {
                (global_relative.clone(), None, global_candidate.clone())
            } else {
                let authority = project_scope.as_ref().expect("project authority");
                authority.verify()?;
                let manager = PluginManager::open_scoped(
                    &authority.anchor,
                    &authority.store_relative,
                    global_candidate.trusted_signers(),
                )
                .map_err(plugin_manager_error)?;
                (authority.store_relative.clone(), Some(authority.root.clone()), manager)
            };
            let config_path = project_scope
                .as_ref()
                .map(ProjectScopeAuthority::config_path)
                .unwrap_or(config::scope_path(scope, &context.cwd)?);
            let transaction_root = manager.root().to_owned();
            let downloaded = if source.starts_with("https://") {
                Some(download_plugin_package(&source, &execution)?)
            } else {
                None
            };
            let package_path = downloaded
                .as_ref()
                .map(|download| download.file.path())
                .unwrap_or_else(|| Path::new(&source));
            let inspected = manager
                .inspect_file(package_path, sha256.as_deref(), &execution)
                .map_err(plugin_manager_error)?;
            if signing_key_id.as_deref().is_some_and(|id| id != inspected.signing_key_id) {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "pluginSignerMismatch",
                    "package signer differs from --signing-key-id",
                ));
            }
            if let Some(authority) = &project_scope {
                authority.verify()?;
            }
            let config_lock = config::lock_exact(&config_path)?;
            if let Some(authority) = &project_scope {
                authority.verify_config_guard(&config_lock)?;
            }
            let configured_before = config::plugins_in_exact_locked(&config_lock, &config_path)?;
            let old_config = configured_before.get(&inspected.id).cloned();
            let verified_before = manager.verify(&inspected.id, &execution);
            if let Some(configured) = old_config.as_ref()
                && plugin_config_matches_inspected(&manager, configured, &inspected)
            {
                match &verified_before {
                    Ok(installed) => {
                        verify_plugin_pin(&manager, configured, installed)?;
                        if let Some(authority) = &project_scope {
                            authority.verify_config_guard(&config_lock)?;
                        }
                        writeln!(context.stdout, "{}\t{}", installed.id, config_path.display())?;
                        return Ok(());
                    }
                    Err(error) if error.code != ManagerErrorCode::NotInstalled => {
                        // An exact same-authority reinstall is a verification/repair operation.
                        // The manager publishes its staged replacement atomically, while the
                        // unchanged config remains a durable rollback authority. Avoid retaining
                        // a second multi-gigabyte package snapshot solely for a no-op config CAS.
                        let repaired = manager
                            .install_file(package_path, Some(&inspected.package_sha256), &execution)
                            .map_err(plugin_manager_error)?;
                        verify_plugin_pin(&manager, configured, &repaired)?;
                        if let Some(authority) = &project_scope {
                            authority.verify_config_guard(&config_lock)?;
                        }
                        writeln!(context.stdout, "{}\t{}", repaired.id, config_path.display())?;
                        return Ok(());
                    }
                    Err(_) => {}
                }
            }
            let installed_before = match verified_before {
                Ok(installed) => Some(installed),
                Err(error) if error.code == ManagerErrorCode::NotInstalled => None,
                Err(_) => Some(
                    manager
                        .inspect_retained_package(&inspected.id, &execution)
                        .map_err(plugin_manager_error)?,
                ),
            };
            match (old_config.as_ref(), installed_before.as_ref()) {
                (Some(configured), Some(installed)) => {
                    verify_plugin_pin(&manager, configured, installed)?;
                }
                (None, None) => {}
                _ => {
                    return Err(CliError::new(
                        ExitClass::Policy,
                        "pluginTransactionConflict",
                        "plugin store and scope configuration are inconsistent",
                    ));
                }
            }
            let backup_name = format!(
                ".cli-backup-{}-{}-{}.zip",
                inspected.id,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let backup_path = transaction_root.join(&backup_name);
            let _ = fs::remove_file(&backup_path);
            let mut transaction = CliPluginTransaction {
                schema_version: 1,
                operation: CliPluginOperation::Install,
                phase: CliPluginPhase::Started,
                global: scope == crate::args::Scope::Global,
                store_relative: store_relative.to_string_lossy().into_owned(),
                project_root,
                id: inspected.id.clone(),
                backup_name: installed_before.as_ref().map(|_| backup_name),
                old_config: old_config.clone(),
                new_config: None,
                signing_key_id: signing_key_id.clone(),
                signing_key_sha256: signing_key_sha256.clone(),
            };
            write_cli_plugin_transaction(&transaction_root, &transaction)?;
            cli_plugin_test_crash("install-started");
            let snapshot = match manager.snapshot_package(&inspected.id, &backup_path, &execution) {
                Ok(snapshot) => Some(snapshot),
                Err(error) if error.code == ManagerErrorCode::NotInstalled => None,
                Err(error) => {
                    clear_cli_plugin_transaction(&transaction_root)?;
                    return Err(plugin_manager_error(error));
                }
            };
            let installed = match manager.install_file(
                package_path,
                Some(&inspected.package_sha256),
                &execution,
            ) {
                Ok(installed) => installed,
                Err(error) => {
                    rollback_plugin_store(
                        &manager,
                        &inspected.id,
                        snapshot.as_ref().map(|value| value.path()),
                        snapshot.as_ref().map(|value| value.installed().package_sha256.as_str()),
                        &execution,
                    )?;
                    drop(snapshot);
                    clear_cli_plugin_transaction(&transaction_root)?;
                    return Err(plugin_manager_error(error));
                }
            };
            let key_id = signing_key_id.clone().unwrap_or(installed.signing_key_id.clone());
            let key_sha256 = signing_key_sha256
                .clone()
                .or_else(|| manager.trusted_signers().fingerprints.get(&key_id).cloned())
                .ok_or_else(|| CliError::usage("publisher trust requires signing key options"))?;
            let new_config = PluginConfig {
                source,
                sha256: Some(installed.package_sha256.clone()),
                protocol: installed.protocol.clone(),
                enabled: true,
                signing_key_id: key_id,
                signing_key_sha256: key_sha256,
            };
            // StoreChanged is also the durable intent for the exact config CAS.
            // Recovery can therefore undo a CAS even if the process dies before
            // the subsequent ConfigChanged journal rewrite.
            transaction.new_config = Some(new_config.clone());
            transaction.phase = CliPluginPhase::StoreChanged;
            if let Err(error) = write_cli_plugin_transaction(&transaction_root, &transaction) {
                rollback_plugin_store(
                    &manager,
                    &installed.id,
                    snapshot.as_ref().map(|value| value.path()),
                    snapshot.as_ref().map(|value| value.installed().package_sha256.as_str()),
                    &execution,
                )?;
                drop(snapshot);
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            cli_plugin_test_crash("install-store-changed");
            if let Some(snapshot) = snapshot {
                let _ = snapshot.persist();
            }
            if let Some(authority) = &project_scope {
                authority.verify_config_guard(&config_lock)?;
            }
            let path = match config::compare_and_set_plugin_exact_locked(
                &config_lock,
                &config_path,
                &installed.id,
                old_config.as_ref(),
                Some(&new_config),
            ) {
                Ok(path) => path,
                Err(error) => {
                    rollback_plugin_store(
                        &manager,
                        &installed.id,
                        old_config.as_ref().map(|_| backup_path.as_path()),
                        old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                        &execution,
                    )?;
                    clear_cli_plugin_transaction(&transaction_root)?;
                    return Err(error);
                }
            };
            cli_plugin_test_crash("install-config-cas");
            if let Some(authority) = &project_scope
                && let Err(error) = authority.verify_config_guard(&config_lock)
            {
                rollback_plugin_store(
                    &manager,
                    &installed.id,
                    old_config.as_ref().map(|_| backup_path.as_path()),
                    old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                    &execution,
                )?;
                restore_plugin_config(
                    &config_lock,
                    &config_path,
                    &installed.id,
                    Some(&new_config),
                    old_config.as_ref(),
                )?;
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            transaction.phase = CliPluginPhase::ConfigChanged;
            if let Err(error) = write_cli_plugin_transaction(&transaction_root, &transaction) {
                rollback_plugin_store(
                    &manager,
                    &installed.id,
                    old_config.as_ref().map(|_| backup_path.as_path()),
                    old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                    &execution,
                )?;
                restore_plugin_config(
                    &config_lock,
                    &config_path,
                    &installed.id,
                    Some(&new_config),
                    old_config.as_ref(),
                )?;
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            cli_plugin_test_crash("install-config-changed");
            let mut trust_pending = false;
            if scope == crate::args::Scope::Global
                && let (Some(id), Some(fingerprint)) =
                    (signing_key_id.as_deref(), signing_key_sha256.as_deref())
                && {
                    arm_plugin_manager_trust_fault(global_manager.root())?;
                    true
                }
                && let Err(error) = global_manager.trust_signer(id, fingerprint)
            {
                if error.code == ManagerErrorCode::Indeterminate {
                    // ConfigChanged is a durable forward-commit intent.  Keep
                    // the joint journal so startup can finish trust publication;
                    // rolling back here could leave a pending signer expansion
                    // that becomes visible on the next process.
                    trust_pending = true;
                } else {
                    rollback_plugin_store(
                        &manager,
                        &installed.id,
                        old_config.as_ref().map(|_| backup_path.as_path()),
                        old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                        &execution,
                    )?;
                    restore_plugin_config(
                        &config_lock,
                        &config_path,
                        &installed.id,
                        Some(&new_config),
                        old_config.as_ref(),
                    )?;
                    clear_cli_plugin_transaction(&transaction_root)?;
                    return Err(plugin_manager_error(error));
                }
            }
            if trust_pending {
                writeln!(context.stdout, "{}\t{}", installed.id, path.display())?;
                return Ok(());
            }
            cli_plugin_test_crash("install-trust-published");
            transaction.phase = CliPluginPhase::TrustChanged;
            if write_cli_plugin_transaction(&transaction_root, &transaction).is_ok() {
                let backup_clean = !backup_path.exists() || fs::remove_file(&backup_path).is_ok();
                if backup_clean {
                    let _ = clear_cli_plugin_transaction(&transaction_root);
                }
            }
            writeln!(context.stdout, "{}\t{}", installed.id, path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Verify { id, json, scope }) => {
            let execution = into_markdown::ExecutionContext::new(
                into_markdown::ExecutionOptions::default(),
                into_markdown::ResourceLimits::default(),
            );
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                .map_err(plugin_manager_error)?;
            let authority = scoped_plugin_authority(scope, &context.cwd, &global)?;
            let config_lock = lock_scoped_plugin_config(&authority)?;
            let configured = config::plugins_in_exact_locked(&config_lock, &authority.config_path)?;
            let manager = &authority.manager;
            let verified = if let Some(id) = id {
                let installed = manager.verify(&id, &execution).map_err(plugin_manager_error)?;
                let authority = configured.get(&id).ok_or_else(|| {
                    CliError::config(format!(
                        "plugin '{id}' is installed but not configured in this scope"
                    ))
                })?;
                verify_plugin_pin(&manager, authority, &installed)?;
                vec![installed]
            } else {
                let installed = manager.verify_all(&execution).map_err(plugin_manager_error)?;
                for plugin in &installed {
                    let authority = configured.get(&plugin.id).ok_or_else(|| {
                        CliError::config(format!(
                            "plugin '{}' is installed but not configured in this scope",
                            plugin.id
                        ))
                    })?;
                    verify_plugin_pin(&manager, authority, plugin)?;
                }
                for id in configured.keys() {
                    if !installed.iter().any(|plugin| &plugin.id == id) {
                        return Err(CliError::config(format!(
                            "plugin '{id}' is configured but not installed in this scope"
                        )));
                    }
                }
                installed
            };
            if let Some(project) = &authority.project {
                project.verify_config_guard(&config_lock)?;
            }
            if json {
                write_json(context.stdout, &verified)
            } else {
                for plugin in verified {
                    writeln!(
                        context.stdout,
                        "{}\t{}\t{}",
                        plugin.id, plugin.version, plugin.protocol
                    )?;
                }
                Ok(())
            }
        }
        Some(PluginsCommand::Enable { id, scope }) => {
            let execution = plugin_lifecycle_execution_context();
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let mut global_manager =
                PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                    .map_err(plugin_manager_error)?;
            let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
            recover_pending_cli_plugin_transaction(
                &global_anchor,
                &global_relative,
                &mut global_manager,
                &execution,
            )?;
            let authority = scoped_plugin_authority(scope, &context.cwd, &global_manager)?;
            let manager = &authority.manager;
            let installed = manager.verify(&id, &execution).map_err(plugin_manager_error)?;
            let config_lock = lock_scoped_plugin_config(&authority)?;
            let configured_by_scope =
                config::plugins_in_exact_locked(&config_lock, &authority.config_path)?;
            let configured = configured_by_scope
                .get(&id)
                .ok_or_else(|| CliError::config(format!("plugin '{id}' is not configured")))?;
            verify_plugin_pin(&manager, configured, &installed)?;
            let mut enabled = configured.clone();
            enabled.enabled = true;
            let path = config::compare_and_set_plugin_exact_locked(
                &config_lock,
                &authority.config_path,
                &id,
                Some(configured),
                Some(&enabled),
            )?;
            if let Some(project) = &authority.project
                && let Err(error) = project.verify_config_guard(&config_lock)
            {
                config::compare_and_set_plugin_exact_locked(
                    &config_lock,
                    &authority.config_path,
                    &id,
                    Some(&enabled),
                    Some(configured),
                )?;
                return Err(error);
            }
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Disable { id, scope }) => {
            let execution = plugin_lifecycle_execution_context();
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let mut global_manager =
                PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                    .map_err(plugin_manager_error)?;
            let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
            recover_pending_cli_plugin_transaction(
                &global_anchor,
                &global_relative,
                &mut global_manager,
                &execution,
            )?;
            let authority = scoped_plugin_authority(scope, &context.cwd, &global_manager)?;
            let config_lock = lock_scoped_plugin_config(&authority)?;
            let configured = config::plugins_in_exact_locked(&config_lock, &authority.config_path)?;
            let current = configured
                .get(&id)
                .ok_or_else(|| CliError::config(format!("plugin '{id}' is not configured")))?;
            let mut disabled = current.clone();
            disabled.enabled = false;
            let path = config::compare_and_set_plugin_exact_locked(
                &config_lock,
                &authority.config_path,
                &id,
                Some(current),
                Some(&disabled),
            )?;
            if let Some(project) = &authority.project
                && let Err(error) = project.verify_config_guard(&config_lock)
            {
                config::compare_and_set_plugin_exact_locked(
                    &config_lock,
                    &authority.config_path,
                    &id,
                    Some(&disabled),
                    Some(current),
                )?;
                return Err(error);
            }
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Remove { id, scope }) => {
            let execution = plugin_lifecycle_execution_context();
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let mut global_manager =
                PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                    .map_err(plugin_manager_error)?;
            let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
            recover_pending_cli_plugin_transaction(
                &global_anchor,
                &global_relative,
                &mut global_manager,
                &execution,
            )?;
            let project_scope = if scope == crate::args::Scope::Project {
                Some(ProjectScopeAuthority::resolve(&context.cwd)?)
            } else {
                None
            };
            let (store_relative, project_root, manager) = if scope == crate::args::Scope::Global {
                (global_relative, None, global_manager.clone())
            } else {
                let authority = project_scope.as_ref().expect("project authority");
                authority.verify()?;
                let manager = PluginManager::open_scoped(
                    &authority.anchor,
                    &authority.store_relative,
                    global_manager.trusted_signers(),
                )
                .map_err(plugin_manager_error)?;
                (authority.store_relative.clone(), Some(authority.root.clone()), manager)
            };
            let config_path = project_scope
                .as_ref()
                .map(ProjectScopeAuthority::config_path)
                .unwrap_or(config::scope_path(scope, &context.cwd)?);
            let transaction_root = manager.root().to_owned();
            if let Some(authority) = &project_scope {
                authority.verify()?;
            }
            let config_lock = config::lock_exact(&config_path)?;
            if let Some(authority) = &project_scope {
                authority.verify_config_guard(&config_lock)?;
            }
            let configured = config::plugins_in_exact_locked(&config_lock, &config_path)?;
            let old_config = configured.get(&id).cloned();
            let installed_before = manager.verify(&id, &execution).map_err(plugin_manager_error)?;
            let configured_before = old_config.as_ref().ok_or_else(|| {
                CliError::new(
                    ExitClass::Policy,
                    "pluginTransactionConflict",
                    "plugin is installed but absent from scope configuration",
                )
            })?;
            verify_plugin_pin(&manager, configured_before, &installed_before)?;
            let backup_name = format!(
                ".cli-backup-{}-{}-{}.zip",
                id,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let backup_path = transaction_root.join(&backup_name);
            let _ = fs::remove_file(&backup_path);
            let mut transaction = CliPluginTransaction {
                schema_version: 1,
                operation: CliPluginOperation::Remove,
                phase: CliPluginPhase::Started,
                global: scope == crate::args::Scope::Global,
                store_relative: store_relative.to_string_lossy().into_owned(),
                project_root,
                id: id.clone(),
                backup_name: Some(backup_name),
                old_config: old_config.clone(),
                new_config: None,
                signing_key_id: None,
                signing_key_sha256: None,
            };
            write_cli_plugin_transaction(&transaction_root, &transaction)?;
            cli_plugin_test_crash("remove-started");
            let snapshot =
                manager.snapshot_package(&id, &backup_path, &execution).map_err(|error| {
                    let _ = clear_cli_plugin_transaction(&transaction_root);
                    plugin_manager_error(error)
                })?;
            if let Err(error) = manager.remove(&id) {
                drop(snapshot);
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(plugin_manager_error(error));
            }
            transaction.phase = CliPluginPhase::StoreChanged;
            if let Err(error) = write_cli_plugin_transaction(&transaction_root, &transaction) {
                rollback_plugin_store(
                    &manager,
                    &id,
                    Some(snapshot.path()),
                    Some(&snapshot.installed().package_sha256),
                    &execution,
                )?;
                drop(snapshot);
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            cli_plugin_test_crash("remove-store-changed");
            let _ = snapshot.persist();
            if let Some(authority) = &project_scope {
                authority.verify_config_guard(&config_lock)?;
            }
            let path = match config::compare_and_set_plugin_exact_locked(
                &config_lock,
                &config_path,
                &id,
                old_config.as_ref(),
                None,
            ) {
                Ok(path) => path,
                Err(error) => {
                    rollback_plugin_store(
                        &manager,
                        &id,
                        Some(&backup_path),
                        old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                        &execution,
                    )?;
                    clear_cli_plugin_transaction(&transaction_root)?;
                    return Err(error);
                }
            };
            cli_plugin_test_crash("remove-config-cas");
            if let Some(authority) = &project_scope
                && let Err(error) = authority.verify_config_guard(&config_lock)
            {
                rollback_plugin_store(
                    &manager,
                    &id,
                    Some(&backup_path),
                    old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                    &execution,
                )?;
                restore_plugin_config(&config_lock, &config_path, &id, None, old_config.as_ref())?;
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            transaction.phase = CliPluginPhase::ConfigChanged;
            if let Err(error) = write_cli_plugin_transaction(&transaction_root, &transaction) {
                rollback_plugin_store(
                    &manager,
                    &id,
                    Some(&backup_path),
                    old_config.as_ref().and_then(|value| value.sha256.as_deref()),
                    &execution,
                )?;
                restore_plugin_config(&config_lock, &config_path, &id, None, old_config.as_ref())?;
                clear_cli_plugin_transaction(&transaction_root)?;
                return Err(error);
            }
            cli_plugin_test_crash("remove-config-changed");
            let backup_clean = !backup_path.exists() || fs::remove_file(&backup_path).is_ok();
            if backup_clean {
                let _ = clear_cli_plugin_transaction(&transaction_root);
            }
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Run { id, input, input_format, scope }) => {
            let execution = into_markdown::ExecutionContext::new(
                into_markdown::ExecutionOptions::default(),
                into_markdown::ResourceLimits::default(),
            );
            let (global_anchor, global_relative) = global_plugin_store_scope()?;
            let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
                .map_err(plugin_manager_error)?;
            let authority = scoped_plugin_authority(scope, &context.cwd, &global)?;
            let config_lock = lock_scoped_plugin_config(&authority)?;
            let configured_by_scope =
                config::plugins_in_exact_locked(&config_lock, &authority.config_path)?;
            let manager = &authority.manager;
            let configured =
                configured_by_scope.get(&id).filter(|plugin| plugin.enabled).ok_or_else(|| {
                    CliError::config(format!("plugin '{id}' is not enabled in this scope"))
                })?;
            let installed = manager.verify(&id, &execution).map_err(plugin_manager_error)?;
            verify_plugin_pin(&manager, configured, &installed)?;
            if let Some(project) = &authority.project {
                project.verify_config_guard(&config_lock)?;
            }
            let maximum = execution.resource_limits().max_input_bytes;
            let (source, _input_memory) = read_plugin_input(&input, maximum, &execution)?;
            match configured.protocol.as_str() {
                "process-v1" => {
                    let prepared = manager
                        .process_manifest(
                            &id,
                            into_markdown_process_plugin::RuntimePolicy::default(),
                            &execution,
                        )
                        .map_err(plugin_manager_error)?;
                    let result = prepared
                        .execute(
                            into_markdown_process_plugin::PluginRequest {
                                request_id: "cli-plugin-run",
                                input_format: &input_format,
                                source_name: input.file_name().and_then(OsStr::to_str),
                                parameters_json: None,
                                source: &source,
                            },
                            &execution,
                        )
                        .map_err(process_plugin_error)?;
                    let encoded = output::encode_result(&result.result, EmitKind::ResultJson)?;
                    context.stdout.write_all(&encoded).map_err(CliError::from)
                }
                "wasi-v1" => {
                    let prepared = manager
                        .prepare_wasi(
                            &id,
                            &into_markdown_plugin_wasi::WasiCapabilities::default(),
                            &execution,
                        )
                        .map_err(plugin_manager_error)?;
                    let request = into_markdown_plugin_wasi::PluginRequest {
                        protocol_version: into_markdown_plugin_wasi::PROTOCOL_VERSION,
                        source_name: input
                            .file_name()
                            .and_then(OsStr::to_str)
                            .unwrap_or("input")
                            .to_owned(),
                        input: source,
                    };
                    // Wasmtime's synchronous component adapter can exceed the
                    // Windows main-thread stack even for a bounded guest. Keep
                    // the reviewed runtime limits and give only the host call
                    // stack a fixed, cross-platform bound.
                    let result = std::thread::scope(|scope| {
                        std::thread::Builder::new()
                            .name("into-md-wasi-plugin".into())
                            .stack_size(8 * 1024 * 1024)
                            .spawn_scoped(scope, || prepared.execute(&request, &execution))
                            .map_err(|error| {
                                CliError::internal(format!("start WASI plugin thread: {error}"))
                            })?
                            .join()
                            .map_err(|_| CliError::internal("WASI plugin thread panicked"))?
                            .map_err(wasi_plugin_error)
                    })?;
                    let document = serde_json::from_str::<serde_json::Value>(
                        &result.document.to_json().map_err(|error| {
                            CliError::internal(format!("serialize plugin document: {error}"))
                        })?,
                    )
                    .map_err(|error| {
                        CliError::internal(format!("serialize plugin document: {error}"))
                    })?;
                    write_json(
                        context.stdout,
                        &serde_json::json!({ "document": document, "resources": result.resources }),
                    )
                }
                _ => Err(CliError::new(
                    ExitClass::Policy,
                    "unsupportedProtocol",
                    "plugin protocol is unsupported",
                )),
            }
        }
    }
}

fn rollback_plugin_store(
    manager: &PluginManager,
    id: &str,
    backup: Option<&Path>,
    expected_sha256: Option<&str>,
    execution: &into_markdown::ExecutionContext,
) -> Result<(), CliError> {
    if let Some(backup) = backup.filter(|path| path.is_file()) {
        manager.install_file(backup, expected_sha256, execution).map_err(plugin_manager_error)?;
    } else if let Err(error) = manager.remove(id)
        && error.code != ManagerErrorCode::NotInstalled
    {
        return Err(plugin_manager_error(error));
    }
    Ok(())
}

fn restore_plugin_config(
    lock: &config::ConfigMutationGuard,
    config_path: &Path,
    id: &str,
    changed: Option<&PluginConfig>,
    previous: Option<&PluginConfig>,
) -> Result<(), CliError> {
    let current = config::plugins_in_exact_locked(lock, config_path)?;
    if current.get(id) == previous {
        return Ok(());
    }
    config::compare_and_set_plugin_exact_locked(lock, config_path, id, changed, previous)?;
    Ok(())
}

struct CliPluginLock(File);

impl Drop for CliPluginLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn acquire_cli_plugin_lock(root: &Path) -> Result<CliPluginLock, CliError> {
    let path = root.join(PLUGIN_CLI_LOCK);
    let file =
        OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let linked = metadata.file_type().is_symlink();
    if !metadata.is_file() || linked {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction lock identity rejected",
        ));
    }
    file.try_lock().map_err(|_| {
        CliError::new(
            ExitClass::Policy,
            "pluginTransactionConflict",
            "another plugin transaction is active",
        )
    })?;
    Ok(CliPluginLock(file))
}

fn recover_cli_transaction_files(root: &Path) -> Result<(), CliError> {
    let journal = root.join(PLUGIN_CLI_TRANSACTION);
    let next = root.join(PLUGIN_CLI_TRANSACTION_NEXT);
    let previous = root.join(PLUGIN_CLI_TRANSACTION_PREVIOUS);
    if journal.exists() {
        let _ = fs::remove_file(&next);
        let _ = fs::remove_file(&previous);
    } else if next.exists() {
        fs::rename(&next, &journal)?;
        let _ = fs::remove_file(&previous);
    } else if previous.exists() {
        fs::rename(&previous, &journal)?;
    }
    sync_cli_plugin_directory(root)?;
    Ok(())
}

fn sync_cli_plugin_directory(root: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

fn write_cli_plugin_transaction(
    root: &Path,
    transaction: &CliPluginTransaction,
) -> Result<(), CliError> {
    recover_cli_transaction_files(root)?;
    let journal = root.join(PLUGIN_CLI_TRANSACTION);
    let next = root.join(PLUGIN_CLI_TRANSACTION_NEXT);
    let previous = root.join(PLUGIN_CLI_TRANSACTION_PREVIOUS);
    let bytes = serde_json::to_vec(transaction)
        .map_err(|error| CliError::internal(format!("serialize plugin transaction: {error}")))?;
    let mut output = OpenOptions::new().create_new(true).write(true).open(&next)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    if journal.exists() {
        fs::rename(&journal, &previous)?;
        sync_cli_plugin_directory(root)?;
    }
    if let Err(error) = fs::rename(&next, &journal) {
        if previous.exists() {
            let _ = fs::rename(&previous, &journal);
        }
        return Err(error.into());
    }
    sync_cli_plugin_directory(root)?;
    let _ = fs::remove_file(previous);
    sync_cli_plugin_directory(root)?;
    Ok(())
}

fn clear_cli_plugin_transaction(root: &Path) -> Result<(), CliError> {
    for name in
        [PLUGIN_CLI_TRANSACTION, PLUGIN_CLI_TRANSACTION_NEXT, PLUGIN_CLI_TRANSACTION_PREVIOUS]
    {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    sync_cli_plugin_directory(root)?;
    Ok(())
}

#[cfg(test)]
fn cli_plugin_test_crash(point: &str) {
    if std::env::var_os("INTO_MD_PLUGIN_CLI_CRASH_POINT").as_deref()
        == Some(std::ffi::OsStr::new(point))
    {
        std::process::exit(86);
    }
}

#[cfg(not(test))]
fn cli_plugin_test_crash(_point: &str) {}

#[cfg(all(test, feature = "plugin-manager-fault-injection"))]
fn arm_plugin_manager_trust_fault(root: &Path) -> Result<(), CliError> {
    if std::env::var_os("INTO_MD_PLUGIN_TRUST_INDETERMINATE").is_some() {
        fs::write(root.join(".test-fail-atomic-rename"), b"2")?;
    }
    Ok(())
}

#[cfg(not(all(test, feature = "plugin-manager-fault-injection")))]
fn arm_plugin_manager_trust_fault(_root: &Path) -> Result<(), CliError> {
    Ok(())
}

fn read_cli_plugin_transaction(root: &Path) -> Result<Option<CliPluginTransaction>, CliError> {
    recover_cli_transaction_files(root)?;
    let path = root.join(PLUGIN_CLI_TRANSACTION);
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction journal rejected",
        ));
    }
    let bytes = fs::read(path)?;
    let transaction: CliPluginTransaction = serde_json::from_slice(&bytes).map_err(|_| {
        CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction journal is invalid",
        )
    })?;
    if transaction.schema_version != 1
        || transaction.id.is_empty()
        || transaction
            .backup_name
            .as_deref()
            .is_some_and(|name| !name.starts_with(".cli-backup-") || name.contains(['/', '\\']))
        || Path::new(&transaction.store_relative)
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction authority rejected",
        ));
    }
    Ok(Some(transaction))
}

fn recover_pending_cli_plugin_transaction(
    global_anchor: &Path,
    global_relative: &Path,
    global_manager: &mut PluginManager,
    execution: &into_markdown::ExecutionContext,
) -> Result<(), CliError> {
    let root = global_manager.root().to_owned();
    recover_pending_cli_plugin_transaction_at(
        &root,
        global_anchor,
        global_relative,
        global_manager,
        execution,
    )
}

fn recover_pending_cli_plugin_transaction_at(
    journal_root: &Path,
    global_anchor: &Path,
    global_relative: &Path,
    global_manager: &mut PluginManager,
    execution: &into_markdown::ExecutionContext,
) -> Result<(), CliError> {
    let root = journal_root.to_owned();
    let Some(transaction) = read_cli_plugin_transaction(&root)? else {
        return Ok(());
    };
    let scope =
        if transaction.global { crate::args::Scope::Global } else { crate::args::Scope::Project };
    let cwd = transaction.project_root.as_deref().unwrap_or(global_anchor);
    let project_scope =
        if transaction.global { None } else { Some(ProjectScopeAuthority::resolve(cwd)?) };
    let expected_relative = if transaction.global {
        global_relative.to_owned()
    } else {
        let authority = project_scope.as_ref().expect("project authority");
        authority.verify()?;
        if authority.anchor != global_anchor {
            return Err(CliError::new(
                ExitClass::Policy,
                "pluginTransactionAuthority",
                "project transaction anchor changed",
            ));
        }
        authority.store_relative.clone()
    };
    if Path::new(&transaction.store_relative) != expected_relative {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction scope changed",
        ));
    }
    let config_path = project_scope
        .as_ref()
        .map(ProjectScopeAuthority::config_path)
        .unwrap_or(config::scope_path(scope, cwd)?);
    let config_lock = config::lock_exact(&config_path)?;
    if let Some(authority) = &project_scope {
        authority.verify_config_guard(&config_lock)?;
    }
    let manager = if transaction.global {
        global_manager.clone()
    } else {
        PluginManager::open_scoped(
            global_anchor,
            &expected_relative,
            global_manager.trusted_signers(),
        )
        .map_err(plugin_manager_error)?
    };
    let manager = if matches!(transaction.operation, CliPluginOperation::Install)
        && let (Some(id), Some(fingerprint)) =
            (transaction.signing_key_id.as_deref(), transaction.signing_key_sha256.as_deref())
    {
        manager.with_candidate_signer(id, fingerprint).map_err(plugin_manager_error)?
    } else {
        manager
    };
    if manager.root() != root {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction journal is outside its bound scope store",
        ));
    }
    let backup = transaction.backup_name.as_deref().map(|name| root.join(name));
    let committed = match transaction.operation {
        CliPluginOperation::Install => {
            matches!(
                transaction.phase,
                CliPluginPhase::ConfigChanged | CliPluginPhase::TrustChanged
            )
        }
        CliPluginOperation::Remove => transaction.phase == CliPluginPhase::ConfigChanged,
    };
    if let Some(authority) = &project_scope {
        authority.verify()?;
    }
    if committed {
        if matches!(transaction.operation, CliPluginOperation::Install) {
            let installed =
                manager.verify(&transaction.id, execution).map_err(plugin_manager_error)?;
            let configured = config::plugins_in_exact_locked(&config_lock, &config_path)?;
            let expected = transaction.new_config.as_ref().ok_or_else(|| {
                CliError::new(
                    ExitClass::Policy,
                    "pluginTransactionAuthority",
                    "committed install lacks configuration authority",
                )
            })?;
            if configured.get(&transaction.id) != Some(expected) {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "pluginTransactionRecovery",
                    "committed plugin configuration changed",
                ));
            }
            verify_plugin_pin(&manager, expected, &installed)?;
        }
        if matches!(transaction.operation, CliPluginOperation::Install)
            && transaction.global
            && let (Some(id), Some(fingerprint)) =
                (transaction.signing_key_id.as_deref(), transaction.signing_key_sha256.as_deref())
        {
            global_manager.trust_signer(id, fingerprint).map_err(plugin_manager_error)?;
        }
    } else {
        if let Some(backup) = backup.as_deref().filter(|path| path.is_file()) {
            let expected =
                transaction.old_config.as_ref().and_then(|plugin| plugin.sha256.as_deref());
            if let Err(error) = manager.install_file(backup, expected, execution) {
                // A crash while creating a pre-change snapshot may leave an
                // unauthoritative file on older stores.  It is safe to discard
                // only when the live store still proves the exact old package;
                // otherwise the backup is required for rollback and corruption
                // remains a fail-closed recovery error.
                let old_is_live = expected.is_some_and(|sha256| {
                    manager
                        .verify(&transaction.id, execution)
                        .is_ok_and(|installed| installed.package_sha256 == sha256)
                });
                if !old_is_live {
                    return Err(plugin_manager_error(error));
                }
                fs::remove_file(backup)?;
                sync_cli_plugin_directory(&root)?;
            }
        } else if transaction.old_config.is_none() {
            if let Err(error) = manager.remove(&transaction.id)
                && error.code != ManagerErrorCode::NotInstalled
            {
                return Err(plugin_manager_error(error));
            }
        } else {
            let installed =
                manager.verify(&transaction.id, execution).map_err(plugin_manager_error)?;
            if transaction.old_config.as_ref().and_then(|plugin| plugin.sha256.as_deref())
                != Some(installed.package_sha256.as_str())
            {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "pluginTransactionRecovery",
                    "previous plugin package is unavailable for rollback",
                ));
            }
        }
        let changed = match transaction.operation {
            CliPluginOperation::Install => transaction.new_config.as_ref(),
            CliPluginOperation::Remove => None,
        };
        restore_plugin_config(
            &config_lock,
            &config_path,
            &transaction.id,
            changed,
            transaction.old_config.as_ref(),
        )?;
    }
    if let Some(backup) = backup
        && backup.exists()
    {
        fs::remove_file(backup)?;
    }
    clear_cli_plugin_transaction(&root)
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct PluginInputIdentity {
    volume: u64,
    file: u64,
    bytes: u64,
    modified: i128,
}

fn plugin_input_identity(file: &File) -> Result<PluginInputIdentity, CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata().map_err(CliError::from)?;
        Ok(PluginInputIdentity {
            volume: metadata.dev(),
            file: metadata.ino(),
            bytes: metadata.len(),
            modified: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file).map_err(CliError::from)?;
        Ok(PluginInputIdentity {
            volume: information.volume_serial_number(),
            file: information.file_index(),
            bytes: information.file_size(),
            modified: i128::from(information.last_write_time().unwrap_or_default()),
        })
    }
}

fn read_plugin_input(
    path: &Path,
    maximum: u64,
    execution: &into_markdown::ExecutionContext,
) -> Result<(Vec<u8>, into_markdown::ResourceReservation), CliError> {
    #[cfg(unix)]
    let mut file = {
        use std::os::fd::OwnedFd;
        let descriptor: OwnedFd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(CliError::from)?;
        File::from(descriptor)
    };
    #[cfg(windows)]
    let mut file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(CliError::from)?
    };
    let metadata = file.metadata().map_err(CliError::from)?;
    let identity = plugin_input_identity(&file)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "plugin input size rejected",
        ));
    }
    #[cfg(windows)]
    if winapi_util::file::information(&file).map_err(CliError::from)?.file_attributes() & 0x400 != 0
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "pathTraversal",
            "plugin input link rejected",
        ));
    }
    let mut reservation =
        execution.reserve_memory(metadata.len()).map_err(plugin_execution_error)?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| CliError::new(ExitClass::Policy, "resourceLimit", "input size overflow"))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        CliError::new(ExitClass::Policy, "resourceLimit", "input allocation rejected")
    })?;
    if bytes.capacity() > capacity {
        reservation.grow((bytes.capacity() - capacity) as u64).map_err(plugin_execution_error)?;
    }
    let probe = maximum
        .checked_add(1)
        .ok_or_else(|| CliError::new(ExitClass::Policy, "resourceLimit", "input bound overflow"))?;
    let mut bounded = std::io::Read::take(&mut file, probe);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        execution.checkpoint().map_err(plugin_execution_error)?;
        let remaining_with_probe = capacity.saturating_sub(bytes.len()).saturating_add(1);
        let requested = buffer.len().min(remaining_with_probe);
        let read = bounded.read(&mut buffer[..requested]).map_err(CliError::from)?;
        if read == 0 {
            break;
        }
        if bytes.len().checked_add(read).is_none_or(|length| length > capacity) {
            return Err(CliError::new(
                ExitClass::Policy,
                "resourceLimit",
                "plugin input grew beyond limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 != metadata.len() || plugin_input_identity(&file)? != identity {
        return Err(CliError::new(ExitClass::Io, "io", "plugin input changed while reading"));
    }
    Ok((bytes, reservation))
}

fn verify_plugin_pin(
    manager: &PluginManager,
    configured: &PluginConfig,
    installed: &into_markdown_plugin_manager::InstalledPlugin,
) -> Result<(), CliError> {
    let trusted_fingerprint =
        manager.trusted_signers().fingerprints.get(&installed.signing_key_id).cloned();
    if configured.protocol != installed.protocol
        || configured.sha256.as_deref() != Some(installed.package_sha256.as_str())
        || configured.signing_key_id != installed.signing_key_id
        || trusted_fingerprint.as_deref() != Some(configured.signing_key_sha256.as_str())
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginAuthority",
            "scope configuration does not pin the installed package and trusted publisher",
        ));
    }
    Ok(())
}

fn plugin_config_matches_inspected(
    manager: &PluginManager,
    configured: &PluginConfig,
    inspected: &into_markdown_plugin_manager::InspectedPackage,
) -> bool {
    configured.protocol == inspected.protocol
        && configured.sha256.as_deref() == Some(inspected.package_sha256.as_str())
        && configured.signing_key_id == inspected.signing_key_id
        && manager.trusted_signers().fingerprints.get(&inspected.signing_key_id)
            == Some(&configured.signing_key_sha256)
}

/// Verify one captured effective plugin against the store selected by the
/// physical source-layer authority. This deliberately validates the merged
/// pins, not merely the raw configuration in the package's store scope.
pub(crate) fn verify_admin_effective_plugin_with_execution(
    cwd: &Path,
    scope: Scope,
    id: &str,
    configured: &PluginConfig,
    execution: &into_markdown::ExecutionContext,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let authority = scoped_plugin_authority(scope, cwd, &global)?;
    let installed = authority.manager.verify(id, execution).map_err(plugin_manager_error)?;
    verify_plugin_pin(&authority.manager, configured, &installed)?;
    if let Some(project) = &authority.project {
        project.verify()?;
    }
    Ok(installed)
}

/// Authenticate one captured installation record for responsive status
/// display. Large payload hashes remain mandatory in explicit verification
/// and immediately before process execution.
pub(crate) fn inspect_admin_effective_plugin(
    cwd: &Path,
    scope: Scope,
    id: &str,
    configured: &PluginConfig,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    let execution = into_markdown::ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let authority = scoped_plugin_authority(scope, cwd, &global)?;
    let installed =
        authority.manager.inspect_installed_record(id, &execution).map_err(plugin_manager_error)?;
    verify_plugin_pin(&authority.manager, configured, &installed)?;
    if let Some(project) = &authority.project {
        project.verify()?;
    }
    Ok(installed)
}

fn inspect_admin_effective_plugin_status_from_loaded(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<(into_markdown_plugin_manager::InstalledPlugin, String), CliError> {
    let configured = loaded
        .effective
        .plugins
        .get(id)
        .ok_or_else(|| CliError::usage(format!("unknown plugin '{id}'")))?;
    let execution = into_markdown::ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    let scope = admin_effective_plugin_scope(loaded, cwd, id)?;
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let authority = scoped_plugin_authority(scope, cwd, &global)?;
    let (installed, status_token) =
        authority.manager.inspect_installed_status(id, &execution).map_err(plugin_manager_error)?;
    verify_plugin_pin(&authority.manager, configured, &installed)?;
    if let Some(project) = &authority.project {
        project.verify()?;
    }
    Ok((installed, status_token))
}

pub(crate) fn admin_effective_plugin_scope(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<Scope, CliError> {
    let source = loaded.sources.get(&format!("plugins.{id}.source")).ok_or_else(|| {
        CliError::new(
            ExitClass::Policy,
            "pluginAuthority",
            "effective plugin source has no configuration authority",
        )
    })?;
    let project = config::source_is_exact_scope(source, Scope::Project, cwd)?;
    let global = config::source_is_exact_scope(source, Scope::Global, cwd)?;
    match (global, project) {
        (true, false) => Ok(Scope::Global),
        (false, true) => Ok(Scope::Project),
        _ => Err(CliError::new(
            ExitClass::Policy,
            "pluginAuthority",
            "effective plugin source does not identify exactly one managed configuration scope",
        )),
    }
}

pub(crate) fn verify_admin_effective_plugin_from_loaded(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    let execution = into_markdown::ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    verify_admin_effective_plugin_from_loaded_with_execution(loaded, cwd, id, &execution)
}

pub(crate) fn verify_admin_effective_plugin_from_loaded_with_execution(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
    execution: &into_markdown::ExecutionContext,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    let configured = loaded
        .effective
        .plugins
        .get(id)
        .ok_or_else(|| CliError::usage(format!("unknown plugin '{id}'")))?;
    verify_admin_effective_plugin_with_execution(
        cwd,
        admin_effective_plugin_scope(loaded, cwd, id)?,
        id,
        configured,
        execution,
    )
}

pub(crate) fn inspect_admin_effective_plugin_from_loaded(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    let configured = loaded
        .effective
        .plugins
        .get(id)
        .ok_or_else(|| CliError::usage(format!("unknown plugin '{id}'")))?;
    inspect_admin_effective_plugin(
        cwd,
        admin_effective_plugin_scope(loaded, cwd, id)?,
        id,
        configured,
    )
}

/// Prepare one effective process plugin through the same scope and exact-pin authority used by
/// the administration surface. The returned value owns an immutable private runtime snapshot.
pub(crate) fn prepare_admin_effective_process_plugin_from_loaded(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
    policy: into_markdown_process_plugin::RuntimePolicy,
    execution: &into_markdown::ExecutionContext,
) -> Result<into_markdown_plugin_manager::PreparedProcessPlugin, CliError> {
    let configured = loaded
        .effective
        .plugins
        .get(id)
        .ok_or_else(|| CliError::usage(format!("unknown plugin '{id}'")))?;
    if !configured.enabled {
        return Err(CliError::component(format!("plugin '{id}' is disabled")));
    }
    let scope = admin_effective_plugin_scope(loaded, cwd, id)?;
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let global = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let authority = scoped_plugin_authority(scope, cwd, &global)?;
    // A deep verification creates an owner-private immutable runtime snapshot. Reuse that
    // snapshot for later capabilities and conversions in this process; re-checking the signed
    // installation record below makes updates, removals and configuration changes invalidate the
    // key without hashing hundreds of megabytes on every request.
    let installed =
        authority.manager.inspect_installed_record(id, execution).map_err(plugin_manager_error)?;
    verify_plugin_pin(&authority.manager, configured, &installed)?;
    if let Some(project) = &authority.project {
        project.verify()?;
    }
    let cache = PROCESS_SNAPSHOTS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let key = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        installed.root.display(),
        installed.package_sha256,
        installed.content_root_sha256,
        installed.version
    );
    if let Some(prepared) = cache
        .lock()
        .map_err(|_| CliError::internal("plugin runtime cache poisoned"))?
        .get(&key)
        .cloned()
    {
        return Ok(prepared.with_policy(policy));
    }
    let prepared = authority
        .manager
        .process_manifest(id, policy.clone(), execution)
        .map_err(plugin_manager_error)?;
    let mut cache =
        cache.lock().map_err(|_| CliError::internal("plugin runtime cache poisoned"))?;
    cache.retain(|candidate, _| {
        !candidate.starts_with(&format!("{}\u{1f}", installed.root.display()))
    });
    cache.insert(key, prepared.clone());
    Ok(prepared.with_policy(policy))
}

fn global_plugin_store_scope() -> Result<(PathBuf, PathBuf), CliError> {
    #[cfg(test)]
    {
        let anchor = TEST_USER_DATA_ANCHOR
            .with(|slot| slot.borrow().clone())
            .ok_or_else(|| CliError::internal("test user-data anchor was not injected"))?;
        create_private_user_data_directory(&anchor)?;
        let anchor = canonical_private_user_data_directory(&anchor)?;
        let product = anchor.join("into-markdown");
        prepare_user_data_anchor(&product)?;
        Ok((fs::canonicalize(product)?, PathBuf::from("plugins")))
    }
    #[cfg(not(test))]
    {
        if let Some(configured) = std::env::var_os("INTO_MARKDOWN_USER_DATA_HOME") {
            let anchor = PathBuf::from(configured);
            if !anchor.is_absolute() || !anchor.is_dir() {
                return Err(CliError::config(
                    "INTO_MARKDOWN_USER_DATA_HOME must name an existing absolute directory",
                ));
            }
            let anchor = canonical_private_user_data_directory(&anchor)?;
            let product = anchor.join("into-markdown");
            prepare_user_data_anchor(&product)?;
            return Ok((fs::canonicalize(product)?, PathBuf::from("plugins")));
        }
        let config = config::global_config_path()?;
        let directory =
            config.parent().ok_or_else(|| CliError::config("global scope unavailable"))?;
        ensure_private_user_data_path(directory)?;
        if fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err(CliError::config("global config parent link rejected"));
        }
        let parent_identity = project_directory_identity(directory)?;
        let directory = fs::canonicalize(directory)?;
        if project_directory_identity(&directory)? != parent_identity {
            return Err(CliError::config("global config parent identity changed"));
        }
        let product = directory.join("plugin-data");
        prepare_user_data_anchor(&product)?;
        Ok((fs::canonicalize(product)?, PathBuf::from("plugins")))
    }
}

pub(crate) fn prepare_user_data_anchor(path: &Path) -> Result<(), CliError> {
    if !path.exists() {
        create_private_user_data_directory(path)?;
    } else if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(CliError::config("plugin user-data anchor link rejected"));
    }
    let original_identity = project_directory_identity(path)?;
    let canonical = fs::canonicalize(path)?;
    if !fs::metadata(&canonical)?.is_dir() {
        return Err(CliError::config("plugin user-data anchor identity rejected"));
    }
    verify_private_user_data_directory(path)?;
    verify_private_user_data_directory(&canonical)?;
    if project_directory_identity(&canonical)? != original_identity {
        return Err(CliError::config("plugin user-data anchor identity changed"));
    }
    Ok(())
}

fn canonical_private_user_data_directory(path: &Path) -> Result<PathBuf, CliError> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(CliError::config("plugin user-data parent link rejected"));
    }
    verify_private_user_data_directory(path)?;
    let identity = project_directory_identity(path)?;
    let canonical = fs::canonicalize(path)?;
    verify_private_user_data_directory(&canonical)?;
    if project_directory_identity(&canonical)? != identity {
        return Err(CliError::config("plugin user-data parent identity changed"));
    }
    Ok(canonical)
}

fn create_private_user_data_directory(path: &Path) -> Result<(), CliError> {
    if path.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(windows)]
    into_markdown_process_plugin::create_windows_plugin_store_directory(path)
        .map_err(|error| CliError::config(error.to_string()))?;
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
fn ensure_private_user_data_path(path: &Path) -> Result<(), CliError> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor.to_owned());
        cursor = cursor
            .parent()
            .ok_or_else(|| CliError::config("user-data path has no trusted ancestor"))?;
    }
    if fs::symlink_metadata(cursor)?.file_type().is_symlink() {
        return Err(CliError::config("user-data ancestor link rejected"));
    }
    verify_trusted_user_data_parent(cursor)?;
    let identity = project_directory_identity(cursor)?;
    let mut parent = fs::canonicalize(cursor)?;
    verify_trusted_user_data_parent(&parent)?;
    if project_directory_identity(&parent)? != identity {
        return Err(CliError::config("user-data ancestor identity changed"));
    }
    for component in missing.into_iter().rev() {
        let name = component
            .file_name()
            .ok_or_else(|| CliError::config("user-data component rejected"))?;
        let child = parent.join(name);
        create_private_user_data_directory(&child)?;
        let canonical = canonical_private_user_data_directory(&child)?;
        if canonical.parent() != Some(parent.as_path()) {
            return Err(CliError::config("user-data component escaped trusted parent"));
        }
        parent = canonical;
    }
    Ok(())
}

fn verify_private_user_data_directory(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fs::metadata(path)?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(CliError::config("plugin user-data anchor permissions rejected"));
        }
    }
    #[cfg(windows)]
    into_markdown_process_plugin::verify_windows_plugin_store_path(path)
        .map_err(|error| CliError::config(error.to_string()))?;
    Ok(())
}

#[cfg(windows)]
fn verify_trusted_user_data_parent(path: &Path) -> Result<(), CliError> {
    into_markdown_process_plugin::verify_windows_plugin_trusted_parent(path)
        .map_err(|error| CliError::config(error.to_string()))
}

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
fn verify_trusted_user_data_parent(path: &Path) -> Result<(), CliError> {
    verify_private_user_data_directory(path)
}

#[derive(Clone)]
struct ProjectScopeAuthority {
    root: PathBuf,
    volume: u64,
    file: u64,
    anchor: PathBuf,
    store_relative: PathBuf,
}

impl ProjectScopeAuthority {
    fn resolve(cwd: &Path) -> Result<Self, CliError> {
        let project = config::project_scope_root(cwd)?;
        let (volume, file) = project_directory_identity(&project)?;
        let mut authority = b"into-markdown/project-plugin-scope/v1\0".to_vec();
        authority.extend_from_slice(&volume.to_le_bytes());
        authority.extend_from_slice(&file.to_le_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            authority.extend_from_slice(project.as_os_str().as_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            for unit in project.as_os_str().encode_wide() {
                authority.extend_from_slice(&unit.to_le_bytes());
            }
        }
        let key = format!("{:x}", Sha256::digest(authority));
        let (anchor, global_relative) = global_plugin_store_scope()?;
        let product = global_relative.parent().unwrap_or_else(|| Path::new(""));
        Ok(Self {
            root: project,
            volume,
            file,
            anchor,
            store_relative: product.join("project-plugins").join(key),
        })
    }

    fn verify(&self) -> Result<(), CliError> {
        let canonical = fs::canonicalize(&self.root).map_err(CliError::from)?;
        if canonical != self.root
            || project_directory_identity(&canonical)? != (self.volume, self.file)
        {
            return Err(CliError::new(
                ExitClass::Policy,
                "pluginProjectScopeChanged",
                "project scope identity changed during plugin transaction",
            ));
        }
        Ok(())
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(".into-markdown.toml")
    }

    fn verify_config_guard(&self, guard: &config::ConfigMutationGuard) -> Result<(), CliError> {
        if guard.directory_identity()? != (self.volume, self.file) {
            return Err(CliError::new(
                ExitClass::Policy,
                "pluginProjectScopeChanged",
                "configuration lock is not bound to the resolved project scope",
            ));
        }
        self.verify()
    }
}

fn project_directory_identity(project: &Path) -> Result<(u64, u64), CliError> {
    let metadata = fs::metadata(project).map_err(CliError::from)?;
    if !metadata.is_dir() {
        return Err(CliError::config("project scope is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        return Ok((metadata.dev(), metadata.ino()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&project)
            .map_err(CliError::from)?;
        let information = winapi_util::file::information(&directory).map_err(CliError::from)?;
        if information.file_attributes() & 0x400 != 0 {
            return Err(CliError::config("project scope reparse point rejected"));
        }
        return Ok((information.volume_serial_number(), information.file_index()));
    }
    #[allow(unreachable_code)]
    Err(CliError::config("project scope identity unavailable"))
}

fn project_plugin_store_scope(cwd: &Path) -> Result<(PathBuf, PathBuf), CliError> {
    let authority = ProjectScopeAuthority::resolve(cwd)?;
    Ok((authority.anchor, authority.store_relative))
}

struct ScopedPluginAuthority {
    manager: PluginManager,
    project: Option<ProjectScopeAuthority>,
    config_path: PathBuf,
}

fn scoped_plugin_authority(
    scope: crate::args::Scope,
    cwd: &Path,
    global: &PluginManager,
) -> Result<ScopedPluginAuthority, CliError> {
    if scope == crate::args::Scope::Global {
        return Ok(ScopedPluginAuthority {
            manager: global.clone(),
            project: None,
            config_path: config::scope_path(scope, cwd)?,
        });
    }
    let project = ProjectScopeAuthority::resolve(cwd)?;
    project.verify()?;
    let manager = PluginManager::open_scoped(
        &project.anchor,
        &project.store_relative,
        global.trusted_signers(),
    )
    .map_err(plugin_manager_error)?;
    Ok(ScopedPluginAuthority {
        manager,
        config_path: project.config_path(),
        project: Some(project),
    })
}

fn lock_scoped_plugin_config(
    authority: &ScopedPluginAuthority,
) -> Result<config::ConfigMutationGuard, CliError> {
    let lock = config::lock_exact(&authority.config_path)?;
    if let Some(project) = &authority.project {
        project.verify_config_guard(&lock)?;
    }
    Ok(lock)
}

fn plugin_manager_error(error: ManagerError) -> CliError {
    let (class, code) = match error.code {
        ManagerErrorCode::Cancelled => (ExitClass::Cancelled, "cancelled"),
        ManagerErrorCode::Timeout => (ExitClass::Policy, "timeout"),
        ManagerErrorCode::ResourceLimit => (ExitClass::Policy, "resourceLimit"),
        ManagerErrorCode::Indeterminate => (ExitClass::Io, "transactionIndeterminate"),
        ManagerErrorCode::NotInstalled => (ExitClass::Io, "notFound"),
        ManagerErrorCode::Io => (ExitClass::Io, "io"),
        ManagerErrorCode::Conflict => (ExitClass::Policy, "conflict"),
        ManagerErrorCode::HashMismatch => (ExitClass::Policy, "hashMismatch"),
        ManagerErrorCode::Signature => (ExitClass::Policy, "signature"),
        ManagerErrorCode::UnsupportedProtocol | ManagerErrorCode::UnsupportedTarget => {
            (ExitClass::Component, "componentUnavailable")
        }
        ManagerErrorCode::InvalidPackage | ManagerErrorCode::PathTraversal => {
            (ExitClass::Policy, "invalidPackage")
        }
        _ => (ExitClass::Policy, "pluginManager"),
    };
    CliError::new(class, code, error.to_string())
}

struct DownloadedPlugin {
    file: tempfile::NamedTempFile,
    _temporary: into_markdown::ResourceReservation,
}

fn download_plugin_package(
    source: &str,
    execution: &into_markdown::ExecutionContext,
) -> Result<DownloadedPlugin, CliError> {
    let url = url::Url::parse(source).map_err(|_| CliError::usage("plugin URL is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "invalidPluginUrl",
            "plugin URL must be canonical HTTPS without credentials or fragment",
        ));
    }
    let host = url.host_str().unwrap_or_default().to_owned();
    let policy = NetworkPolicy {
        allow_network: true,
        allow_private_network: false,
        allowed_hosts: vec![host.clone()],
        max_redirects: 0,
    };
    let mut file = tempfile::Builder::new()
        .prefix("into-md-plugin-")
        .suffix(".zip")
        .tempfile()
        .map_err(CliError::from)?;
    let client = crate::proxy_env::model_fetch_client(false)
        .map_err(|(name, detail)| CliError::config(format!("{name}: {detail}")))?;
    let response = client
        .get_to_writer(
            source,
            &policy,
            into_markdown_http_transport::FetchLimits {
                max_wire_bytes: MAX_PLUGIN_PACKAGE_BYTES,
                max_decoded_bytes: MAX_PLUGIN_PACKAGE_BYTES,
            },
            execution,
            file.as_file_mut(),
        )
        .map_err(plugin_transport_error)?;
    if response.bytes_written == 0 {
        return Err(CliError::new(ExitClass::Policy, "invalidPackage", "empty plugin response"));
    }
    file.as_file_mut().sync_all().map_err(CliError::from)?;
    Ok(DownloadedPlugin { file, _temporary: response.into_temporary_reservation() })
}

fn plugin_transport_error(error: TransportError) -> CliError {
    let (class, code) = match error.kind() {
        TransportErrorKind::Cancelled => (ExitClass::Cancelled, "cancelled"),
        TransportErrorKind::Timeout => (ExitClass::Network, "timeout"),
        TransportErrorKind::ResourceLimit => (ExitClass::Policy, "resourceLimit"),
        TransportErrorKind::NetworkDenied
        | TransportErrorKind::HostDenied
        | TransportErrorKind::PrivateNetworkDenied => (ExitClass::Policy, "networkDenied"),
        TransportErrorKind::Dns => (ExitClass::Network, "dns"),
        TransportErrorKind::Connect => (ExitClass::Network, "connect"),
        TransportErrorKind::Tls => (ExitClass::Network, "tls"),
        TransportErrorKind::Http | TransportErrorKind::InvalidMessage => {
            (ExitClass::Network, "invalidHttp")
        }
        TransportErrorKind::Unavailable => (ExitClass::Network, "networkUnavailable"),
        _ => (ExitClass::Network, "pluginDownload"),
    };
    CliError::new(class, code, error.to_string())
}

fn plugin_execution_error(error: into_markdown::ConversionError) -> CliError {
    match error {
        into_markdown::ConversionError::Cancelled => {
            CliError::new(ExitClass::Cancelled, "cancelled", "plugin operation cancelled")
        }
        into_markdown::ConversionError::Timeout => {
            CliError::new(ExitClass::Policy, "timeout", "plugin operation timed out")
        }
        into_markdown::ConversionError::ResourceLimit { .. } => CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "plugin operation exceeded its resource limit",
        ),
        _ => CliError::new(ExitClass::Io, "io", error.to_string()),
    }
}

fn process_plugin_error(error: into_markdown_process_plugin::PluginError) -> CliError {
    use into_markdown_process_plugin::PluginErrorCode as Code;
    let class = match error.code {
        Code::Cancelled => ExitClass::Cancelled,
        Code::SandboxUnavailable | Code::Launch => ExitClass::Component,
        _ => ExitClass::Policy,
    };
    CliError::new(class, error.code.as_str(), error.detail)
}

fn wasi_plugin_error(error: into_markdown_plugin_wasi::WasiPluginError) -> CliError {
    use into_markdown_plugin_wasi::WasiPluginErrorCode as Code;
    let class = match error.code {
        Code::Cancelled => ExitClass::Cancelled,
        Code::Io => ExitClass::Io,
        Code::Runtime | Code::UnsupportedPlatform => ExitClass::Component,
        _ => ExitClass::Policy,
    };
    CliError::new(class, error.code.as_str(), error.detail)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginView<'a> {
    id: &'a str,
    source: String,
    sha256: Option<&'a str>,
    protocol: &'a str,
    enabled: bool,
}

fn run_config(
    command: ConfigCommand,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    match command {
        ConfigCommand::Paths { json } => {
            if json {
                write_json(context.stdout, &loaded.paths)
            } else {
                if let Some(global) = &loaded.paths.global {
                    writeln!(context.stdout, "global: {}", global.display())?;
                } else {
                    writeln!(context.stdout, "global: (disabled)")?;
                }
                writeln!(
                    context.stdout,
                    "project: {}",
                    loaded
                        .paths
                        .project
                        .as_deref()
                        .map_or_else(|| "<none>".into(), |path| path.display().to_string())
                )?;
                for path in &loaded.paths.loaded {
                    writeln!(context.stdout, "loaded: {}", path.display())?;
                }
                Ok(())
            }
        }
        ConfigCommand::Show { resolved, format } => {
            let value = loaded.display_value(resolved)?;
            match format {
                ConfigOutputFormat::Toml => {
                    writeln!(
                        context.stdout,
                        "{}",
                        toml::to_string_pretty(&value).map_err(|error| {
                            CliError::internal(format!("serialize configuration: {error}"))
                        })?
                    )?;
                    Ok(())
                }
                ConfigOutputFormat::Json => write_json(context.stdout, &value),
            }
        }
        ConfigCommand::Init { scope, force } => {
            let path = config::init(scope, &context.cwd, force)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        ConfigCommand::Validate { path } => {
            if let Some(path) = path {
                config::validate_file(&path)?;
            }
            writeln!(context.stdout, "valid")?;
            Ok(())
        }
        ConfigCommand::Get { key } => {
            let value = config::get_value(&loaded.merged, &key)?;
            writeln!(context.stdout, "{value}")?;
            Ok(())
        }
        ConfigCommand::Set { key, value, scope } => {
            let path = config::set(scope, &context.cwd, &key, &value)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        ConfigCommand::Unset { key, scope } => {
            let path = config::unset(scope, &context.cwd, &key)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        ConfigCommand::Profile(arguments) => match arguments.command {
            ProfileCommand::List => {
                for name in config::profile_names(&loaded.merged) {
                    writeln!(context.stdout, "{name}")?;
                }
                Ok(())
            }
            ProfileCommand::Create { name, from, scope } => {
                let path = config::create_profile(
                    scope,
                    &context.cwd,
                    &name,
                    from.as_deref(),
                    &loaded.merged,
                )?;
                writeln!(context.stdout, "{}", path.display())?;
                Ok(())
            }
            ProfileCommand::Remove { name, scope } => {
                let path = config::remove_profile(scope, &context.cwd, &name)?;
                writeln!(context.stdout, "{}", path.display())?;
                Ok(())
            }
        },
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DoctorCheck {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) detail: String,
}

fn run_doctor(
    arguments: &crate::args::DoctorArgs,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    let checks = collect_doctor_checks(arguments, loaded, &context.cwd, arguments.deep);
    if arguments.json {
        write_json(context.stdout, &checks)
    } else {
        writeln!(context.stdout, "CHECK\tSTATUS\tDETAIL")?;
        for check in checks {
            writeln!(
                context.stdout,
                "{}\t{}\t{}",
                doctor_check_name(&check.id),
                check.status,
                check.detail
            )?;
        }
        Ok(())
    }
}

fn doctor_check_name(id: &str) -> String {
    match id {
        "configuration" => "配置".into(),
        "platform" => "运行平台".into(),
        "capabilityDownloads" => "能力下载".into(),
        "temporaryDirectory" => "临时目录".into(),
        "runtime.pdfium" => "PDF 运行组件".into(),
        "runtime.ocr" => "本地 OCR".into(),
        "runtime.asr" => "本地语音转写".into(),
        "runtime.diarization" => "本地说话人识别".into(),
        "networkProbe" => "联网检查".into(),
        value if value.starts_with("providerEnvironment:") => {
            format!("AI 服务 {}", value.trim_start_matches("providerEnvironment:"))
        }
        value if value.starts_with("plugin:") => {
            capability_plugin_name(value.trim_start_matches("plugin:")).into()
        }
        _ => id.into(),
    }
}

pub(crate) fn collect_doctor_checks(
    arguments: &crate::args::DoctorArgs,
    loaded: &LoadedConfig,
    cwd: &Path,
    verify_plugins: bool,
) -> Vec<DoctorCheck> {
    let fast_capabilities = (!verify_plugins).then(|| capability_views(loaded, cwd)).transpose();
    let fast_capabilities = fast_capabilities.ok().flatten();
    let mut checks = vec![
        DoctorCheck {
            id: "configuration".into(),
            status: "ok".into(),
            detail: format!("{} layer(s) loaded", loaded.paths.loaded.len()),
        },
        DoctorCheck {
            id: "platform".into(),
            status: supported_platform_status().0.into(),
            detail: supported_platform_status().1,
        },
        DoctorCheck {
            id: "capabilityDownloads".into(),
            status: match &crate::proxy_env::download_route() {
                crate::proxy_env::DownloadRoute::Direct => "ok".into(),
                crate::proxy_env::DownloadRoute::Proxy { .. } => "ok".into(),
                crate::proxy_env::DownloadRoute::Invalid { .. } => "error".into(),
            },
            detail: match &crate::proxy_env::download_route() {
                crate::proxy_env::DownloadRoute::Direct => {
                    "official capability plugin downloads use direct HTTPS".into()
                }
                crate::proxy_env::DownloadRoute::Proxy { proxy, source, .. } => format!(
                    "capability plugin downloads route HTTPS through the CONNECT proxy from {source}: {}",
                    proxy.redacted_endpoint()
                ),
                crate::proxy_env::DownloadRoute::Invalid { variable, reason } => {
                    format!("invalid {variable}: {reason}")
                }
            },
        },
        DoctorCheck {
            id: "temporaryDirectory".into(),
            status: if std::env::temp_dir().is_dir() { "ok" } else { "error" }.into(),
            detail: std::env::temp_dir().display().to_string(),
        },
    ];
    if verify_plugins {
        append_core_runtime_checks(&mut checks, loaded, cwd);
        let diarization_available =
            crate::services::verify_diarization_runtime(loaded, cwd).is_ok();
        checks.push(DoctorCheck {
            id: "runtime.diarization".into(),
            status: if diarization_available { "ok" } else { "missing" }.into(),
            detail: if diarization_available {
                "the diarization capability in the local speech plugin passed verification".into()
            } else {
                "run `into-md setup media` to install or repair the local speech capability plugin"
                    .into()
            },
        });
    } else {
        append_fast_runtime_checks(&mut checks, loaded, cwd, fast_capabilities.as_deref());
    }
    for (name, provider) in &loaded.effective.providers {
        checks.push(DoctorCheck {
            id: format!("providerEnvironment:{name}"),
            status: if std::env::var_os(&provider.api_key_env).is_some() {
                "ok"
            } else {
                "missing"
            }
            .into(),
            detail: provider.api_key_env.clone(),
        });
    }
    for id in loaded.effective.plugins.keys() {
        let (status, detail) = if verify_plugins {
            doctor_plugin_check(id, loaded, cwd)
        } else if let Some(capabilities) = fast_capabilities.as_deref()
            && let Some(check) = doctor_official_plugin_fast_check(id, capabilities)
        {
            check
        } else {
            doctor_plugin_fast_check(id, loaded, cwd)
        };
        checks.push(DoctorCheck { id: format!("plugin:{id}"), status: status.into(), detail });
    }
    checks.push(DoctorCheck {
        id: "networkProbe".into(),
        status: if arguments.allow_network { "unavailable" } else { "skipped" }.into(),
        detail: if arguments.allow_network {
            "network probe backend is not implemented"
        } else {
            "pass --allow-network to authorize network checks"
        }
        .into(),
    });
    checks
}

fn doctor_plugin_check(id: &str, loaded: &LoadedConfig, cwd: &Path) -> (&'static str, String) {
    let effective = &loaded.effective.plugins[id];
    if !effective.enabled {
        return ("disabled", "plugin is disabled".into());
    }
    match verify_admin_effective_plugin_from_loaded(loaded, cwd, id) {
        Ok(installed) => {
            ("ok", format!("{} runtime package and authority verified", installed.protocol))
        }
        Err(error) => ("error", error.to_string()),
    }
}

fn append_core_runtime_checks(checks: &mut Vec<DoctorCheck>, loaded: &LoadedConfig, cwd: &Path) {
    for capability in into_markdown::core_capabilities()
        .iter()
        .filter(|capability| capability.kind == into_markdown::CapabilityKind::Runtime)
    {
        let Some(runtime) = capability.runtime else {
            checks.push(DoctorCheck {
                id: capability.id.into(),
                status: "error".into(),
                detail: "invalid core runtime catalog entry".into(),
            });
            continue;
        };
        let available = verify_core_runtime(runtime.component, loaded, cwd);
        checks.push(DoctorCheck {
            id: capability.id.into(),
            status: if available { "ok" } else { "missing" }.into(),
            detail: if available {
                format!("{} runtime passed its local authority verification", runtime.component)
            } else {
                runtime.install_hint.into()
            },
        });
    }
}

fn append_fast_runtime_checks(
    checks: &mut Vec<DoctorCheck>,
    loaded: &LoadedConfig,
    cwd: &Path,
    capabilities: Option<&[CapabilityView]>,
) {
    for capability in into_markdown::core_capabilities()
        .iter()
        .filter(|capability| capability.kind == into_markdown::CapabilityKind::Runtime)
    {
        let Some(runtime) = capability.runtime else {
            checks.push(DoctorCheck {
                id: capability.id.into(),
                status: "error".into(),
                detail: "invalid core runtime catalog entry".into(),
            });
            continue;
        };
        if runtime.component == "pdfium" {
            let available = verify_core_runtime(runtime.component, loaded, cwd);
            checks.push(DoctorCheck {
                id: capability.id.into(),
                status: if available { "ok" } else { "missing" }.into(),
                detail: if available {
                    "pdfium runtime passed its local authority verification".into()
                } else {
                    runtime.install_hint.into()
                },
            });
            continue;
        }
        let capability_id = match runtime.component {
            "official.ocr.ppocrv6" => "ocr",
            "official.media.whisper" => "transcription",
            _ => continue,
        };
        append_fast_capability_check(checks, capability.id, capability_id, capabilities);
    }
    append_fast_capability_check(checks, "runtime.diarization", "diarization", capabilities);
}

fn append_fast_capability_check(
    checks: &mut Vec<DoctorCheck>,
    check_id: &str,
    capability_id: &str,
    capabilities: Option<&[CapabilityView]>,
) {
    let Some(capability) = capabilities
        .and_then(|entries| entries.iter().find(|capability| capability.id == capability_id))
    else {
        checks.push(DoctorCheck {
            id: check_id.into(),
            status: "checking".into(),
            detail: "快速状态暂不可用，请重试或运行 `into-md doctor --deep`".into(),
        });
        return;
    };
    checks.push(DoctorCheck {
        id: check_id.into(),
        status: doctor_fast_status(&capability.local_status).into(),
        detail: format!(
            "{}；完整验证请运行 `into-md capabilities verify {}`",
            capability.current_source_name, capability_id
        ),
    });
}

fn doctor_fast_status(status: &str) -> &'static str {
    match status {
        "ready" => "ok",
        "not-installed" => "missing",
        "disabled" => "disabled",
        "checking" | "unknown" => "checking",
        "incompatible" => "incompatible",
        "blocked" => "blocked",
        _ => "error",
    }
}

fn doctor_official_plugin_fast_check(
    id: &str,
    capabilities: &[CapabilityView],
) -> Option<(&'static str, String)> {
    let capability_id = match id {
        "official.ocr.ppocrv6" => "ocr",
        "official.media.whisper" => "transcription",
        _ => return None,
    };
    let capability = capabilities.iter().find(|entry| entry.id == capability_id)?;
    Some((
        doctor_fast_status(&capability.local_status),
        format!("{} 的已认证安装记录", capability_plugin_name(id)),
    ))
}

fn doctor_plugin_fast_check(id: &str, loaded: &LoadedConfig, cwd: &Path) -> (&'static str, String) {
    let effective = &loaded.effective.plugins[id];
    if !effective.enabled {
        return ("disabled", "插件已停用".into());
    }
    match inspect_admin_effective_plugin_from_loaded(loaded, cwd, id) {
        Ok(installed) => ("ok", format!("{} 的安装记录与授权有效", installed.version)),
        Err(error) => ("error", error.to_string()),
    }
}

fn verify_core_runtime(component: &str, loaded: &LoadedConfig, cwd: &Path) -> bool {
    match component {
        "pdfium" => into_markdown::default_pdfium_runtime_path()
            .is_some_and(|path| into_markdown::verify_pdfium_runtime(&path).is_ok()),
        "official.ocr.ppocrv6" => crate::services::verify_ocr_runtime(loaded, cwd).is_ok(),
        "official.media.whisper" => crate::services::verify_asr_runtime(loaded, cwd).is_ok(),
        "speaker-diarization" => crate::services::verify_diarization_runtime(loaded, cwd).is_ok(),
        _ => false,
    }
}

fn supported_platform_status() -> (&'static str, String) {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let supported = matches!(
        (std::env::consts::OS, std::env::consts::ARCH),
        ("macos", "aarch64") | ("linux", "x86_64" | "aarch64") | ("windows", "x86_64")
    );
    (if supported { "ok" } else { "unsupported" }, target)
}

fn generate_completions(shell: CompletionShell, stdout: &mut dyn Write) -> Result<(), CliError> {
    let mut command = Cli::command();
    let generator = match shell {
        CompletionShell::Bash => clap_complete::Shell::Bash,
        CompletionShell::Zsh => clap_complete::Shell::Zsh,
        CompletionShell::Fish => clap_complete::Shell::Fish,
        CompletionShell::Powershell => clap_complete::Shell::PowerShell,
        CompletionShell::Elvish => clap_complete::Shell::Elvish,
    };
    let mut bytes = Vec::new();
    clap_complete::generate(generator, &mut command, "into-md", &mut bytes);
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionView<'a> {
    name: &'a str,
    version: &'a str,
    target_os: &'a str,
    target_arch: &'a str,
    rust_edition: &'a str,
}

fn show_version(json: bool, stdout: &mut dyn Write) -> Result<(), CliError> {
    let view = VersionView {
        name: "into-md",
        version: env!("CARGO_PKG_VERSION"),
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
        rust_edition: "2024",
    };
    if json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "into-md {}", view.version)?;
        writeln!(stdout, "target: {}-{}", view.target_os, view.target_arch)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct WorkItem {
    input: InputRef,
    display: String,
    relative: PathBuf,
    root_label: String,
    input_root: PathBuf,
    from_directory: bool,
    local_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct WorkPlan {
    item: WorkItem,
    output: Option<PathBuf>,
    output_root: Option<PathBuf>,
}

#[derive(Clone)]
struct ExecutionPolicy {
    options: ConversionOptions,
    execution: into_markdown::ExecutionOptions,
    output_context: into_markdown::ExecutionContext,
    services: into_markdown::Services,
    hint: FormatHint,
    emit: EmitKind,
    asset_mode: AssetModeArg,
    conflict: ConflictPolicy,
    assets_dir: Option<PathBuf>,
    working_directory: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
struct AssetOutputPlan {
    uri_prefix: Option<String>,
    external_directory: Option<PathBuf>,
}

fn invocation_capabilities(
    plans: &[WorkPlan],
    explicit_format: Option<InputFormat>,
    extension_hint: Option<&str>,
    options: &ConversionOptions,
) -> crate::services::InvocationCapabilities {
    let hinted = explicit_format.or_else(|| extension_hint.and_then(InputFormat::from_extension));
    let mut formats = Vec::new();
    for plan in plans {
        let format = hinted.or_else(|| {
            plan.item
                .local_path
                .as_deref()
                .and_then(Path::extension)
                .and_then(OsStr::to_str)
                .and_then(InputFormat::from_extension)
        });
        let Some(format) = format else {
            // Stdin and opaque URIs are detected after service assembly. Keep
            // every configured route available rather than guessing.
            return crate::services::InvocationCapabilities {
                ocr: true,
                transcription: true,
                diarization: true,
                legacy_office: true,
            };
        };
        formats.push(format);
    }
    let visual = |format| {
        matches!(
            format,
            InputFormat::Pdf
                | InputFormat::Doc
                | InputFormat::Docx
                | InputFormat::Ppt
                | InputFormat::Pptx
                | InputFormat::Xls
                | InputFormat::Xlsx
                | InputFormat::Odt
                | InputFormat::Ods
                | InputFormat::Odp
                | InputFormat::Epub
                | InputFormat::Html
                | InputFormat::Image
                | InputFormat::OutlookMsg
        )
    };
    let media =
        |format| matches!(format, InputFormat::Audio | InputFormat::Video | InputFormat::YouTube);
    let legacy_office =
        |format| matches!(format, InputFormat::Doc | InputFormat::Ppt | InputFormat::Xls);
    let effective_ocr_policy = effective_ocr_policy(options);
    crate::services::InvocationCapabilities {
        ocr: formats.iter().copied().any(|format| {
            visual(format) && !(effective_ocr_policy == OcrPolicy::Auto && legacy_office(format))
        }),
        transcription: formats.iter().copied().any(media),
        diarization: formats.iter().copied().any(media),
        legacy_office: formats.iter().copied().any(legacy_office),
    }
}

fn effective_ocr_policy(options: &ConversionOptions) -> OcrPolicy {
    match options.ai.vision_ocr {
        AiMode::Only => OcrPolicy::Always,
        AiMode::Fallback | AiMode::Prefer if options.ocr.policy == OcrPolicy::Off => {
            OcrPolicy::Auto
        }
        _ => options.ocr.policy,
    }
}

fn run_conversion(
    arguments: ConversionArgs,
    global: &crate::args::GlobalArgs,
    mut loaded: LoadedConfig,
    catalog: Catalog,
    json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    let batch_timer = crate::timing::ItemTimer::start();
    apply_conversion_overrides(&arguments, &mut loaded)?;
    if loaded.options.limits.max_memory_bytes == 0 {
        return Err(CliError::usage("shared conversion memory budget must be greater than zero"));
    }
    let includes = build_globset(&arguments.include)?;
    let excludes = build_globset(&arguments.exclude)?;
    let mut items = expand_inputs(&arguments, includes.as_ref(), excludes.as_ref())?;
    if items.is_empty() {
        return Err(CliError::usage("no input files matched the requested selection"));
    }
    items.sort_by(|left, right| left.display.cmp(&right.display));
    if (items.len() > 1 || items.iter().any(|item| item.from_directory))
        && arguments.output_dir.is_none()
    {
        return Err(CliError::usage("multiple inputs and directory inputs require --output-dir"));
    }
    if arguments.output.is_some() && items.len() != 1 {
        return Err(CliError::usage("--output may only be used with one input"));
    }
    let emit = arguments.emit.unwrap_or(loaded.emit);
    let asset_mode = arguments.asset_mode.unwrap_or(loaded.asset_mode);
    let conflict = arguments.conflict.unwrap_or(loaded.conflict);
    let mut plans =
        plan_outputs(items, arguments.output.as_deref(), arguments.output_dir.as_deref(), emit);
    disambiguate_planned_outputs(&mut plans);

    if arguments.dry_run {
        writeln!(context.stdout, "INPUT\tOUTPUT\tEMIT")?;
        for plan in &plans {
            writeln!(
                context.stdout,
                "{}\t{}\t{:?}",
                plan.item.display,
                plan.output
                    .as_deref()
                    .map_or_else(|| "<stdout>".into(), |path| path.display().to_string()),
                emit
            )?;
        }
        return Ok(());
    }

    let execution = into_markdown::ExecutionOptions {
        timeout: arguments.timeout_ms.or(loaded.timeout_ms).map(std::time::Duration::from_millis),
        ..into_markdown::ExecutionOptions::default()
    };
    let explicit_format = arguments.format.as_deref().map(parse_format).transpose()?;
    let capability_needs = invocation_capabilities(
        &plans,
        explicit_format,
        arguments.extension.as_deref(),
        &loaded.options,
    );
    let services = crate::services::assemble(&loaded, &execution, &context.cwd, capability_needs)?;
    let output_context =
        into_markdown::ExecutionContext::new(execution.clone(), loaded.options.limits.clone());
    let policy = ExecutionPolicy {
        execution,
        output_context,
        services,
        options: loaded.options,
        hint: FormatHint {
            format: explicit_format,
            extension: arguments.extension,
            media_type: arguments.mime_type,
            charset: arguments.charset,
            ..FormatHint::default()
        },
        emit,
        asset_mode,
        conflict,
        assets_dir: arguments.assets_dir,
        working_directory: context.cwd.clone(),
    };
    let reports = execute_plans(
        plans,
        &policy,
        arguments.jobs.map_or(loaded.jobs, std::num::NonZero::get),
        arguments.report.is_some(),
        catalog,
        json_log,
        context,
    )
    .map_err(|error| error.with_wall_duration(batch_timer.elapsed_ms()))?;
    finish_reports(
        reports,
        FinishReportsContext {
            report_path: arguments.report.as_deref(),
            global,
            catalog,
            json_log,
            stderr: context.stderr,
            output_context: &policy.output_context,
            wall_duration_ms: batch_timer.elapsed_ms(),
            ocr_enabled: effective_ocr_policy(&policy.options) != OcrPolicy::Off,
        },
    )
}

fn execute_plans(
    plans: Vec<WorkPlan>,
    policy: &ExecutionPolicy,
    jobs: usize,
    report_requested: bool,
    catalog: Catalog,
    json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<Vec<BatchItemReport>, CliError> {
    if plans.len() == 1 && plans[0].output.is_none() {
        let item_timer = crate::timing::ItemTimer::start();
        Ok(vec![match process_stdout(&plans[0], policy, catalog, json_log, context) {
            Ok(mut report) => {
                report.duration_ms = Some(item_timer.elapsed_ms());
                report
            }
            Err(error) if report_requested => {
                failed_item_report(&plans[0], policy, &error, Some(item_timer.elapsed_ms()))
            }
            Err(error) => return Err(error.with_duration(item_timer.elapsed_ms())),
        }])
    } else if plans.len() == 1 {
        let plan = &plans[0];
        let output_phase = Mutex::new(());
        let item_timer = crate::timing::ItemTimer::start();
        Ok(vec![match process_file_task_inner(plan, policy, &output_phase) {
            Ok(completed) => {
                completed_item_report(plan.item.display.clone(), completed, item_timer.elapsed_ms())
            }
            Err(error) if report_requested => {
                failed_item_report(plan, policy, &error, Some(item_timer.elapsed_ms()))
            }
            Err(error) => return Err(error.with_duration(item_timer.elapsed_ms())),
        }])
    } else {
        process_batch(plans, policy, jobs)
    }
}

struct FinishReportsContext<'a> {
    report_path: Option<&'a Path>,
    global: &'a crate::args::GlobalArgs,
    catalog: Catalog,
    json_log: bool,
    stderr: &'a mut dyn Write,
    output_context: &'a into_markdown::ExecutionContext,
    wall_duration_ms: f64,
    ocr_enabled: bool,
}

fn finish_reports(
    reports: Vec<BatchItemReport>,
    context: FinishReportsContext<'_>,
) -> Result<(), CliError> {
    let FinishReportsContext {
        report_path,
        global,
        catalog,
        json_log,
        stderr,
        output_context,
        wall_duration_ms,
        ocr_enabled,
    } = context;
    for report in &reports {
        for warning in &report.warnings {
            if !global.quiet {
                write_stderr_event(
                    stderr,
                    json_log,
                    "warning",
                    "outputRenamed",
                    warning,
                    Some(&report.input),
                    &format!("{}: {warning}", catalog.warning_prefix()),
                )?;
            }
        }
        if report.status == BatchItemStatus::Failed && !global.quiet {
            write_stderr_event(
                stderr,
                json_log,
                "error",
                report.error_code.as_deref().unwrap_or("error"),
                report.message.as_deref().unwrap_or("conversion failed"),
                Some(&report.input),
                &format!(
                    "{}: {}: {}",
                    report.input,
                    report.error_code.as_deref().unwrap_or("error"),
                    report.message.as_deref().unwrap_or("conversion failed")
                ),
            )?;
        }
    }
    if !global.quiet {
        crate::timing::write_summary(stderr, &reports, wall_duration_ms, catalog, json_log)?;
    }
    let usage = output_context.resource_usage();
    let resource_usage = into_markdown::BatchResourceUsageDto {
        shared_lease_budget_bytes: usage.shared_lease_budget_bytes,
        shared_lease_peak_bytes: usage.shared_lease_peak_bytes,
        ocr: ocr_enabled.then_some(into_markdown::BatchOcrUsageDto {
            recognized_regions: usage.ocr_recognized_regions,
            recognized_chars: usage.ocr_recognized_chars,
        }),
    };
    let report = BatchReport::try_new_with_resource_usage(
        reports,
        Some(wall_duration_ms),
        Some(resource_usage),
    )
    .map_err(|error| CliError::internal(format!("build batch report DTO: {error}")))?;
    if let Some(path) = report_path {
        let report_context =
            output_context.fork_with_shared_resources(into_markdown::ExecutionOptions::default());
        output::write_report(path, &report, &report_context)?;
    }
    if report.failed > 0 {
        Err(CliError::partial(format!(
            "{} input(s) failed; {} succeeded",
            report.failed, report.succeeded
        )))
    } else {
        Ok(())
    }
}

fn apply_conversion_overrides(
    arguments: &ConversionArgs,
    loaded: &mut LoadedConfig,
) -> Result<(), CliError> {
    if let Some(policy) = arguments.error_policy {
        loaded.options.error_policy = match policy {
            ErrorPolicyArg::BestEffort => ErrorPolicy::BestEffort,
            ErrorPolicyArg::Strict => ErrorPolicy::Strict,
        };
    }
    if let Some(charset) = &arguments.zip_charset {
        loaded.options.archive.zip_charset = Some(charset.clone());
    }
    apply_ocr_overrides(arguments, &mut loaded.options)?;
    apply_asr_overrides(arguments, &mut loaded.options)?;
    apply_text_overrides(arguments, &mut loaded.options);
    apply_network_overrides(arguments, &mut loaded.options)?;
    apply_limit_overrides(arguments, &mut loaded.options);
    apply_output_overrides(arguments, &mut loaded.options);
    apply_ai_capability_overrides(arguments, loaded)?;
    if arguments.diarize && loaded.options.ai.audio_transcription == AiMode::Off {
        return Err(CliError::usage("--diarize conflicts with disabling audio-transcription"));
    }
    if !arguments.ocr_language.is_empty() {
        loaded.ocr_languages.clone_from(&arguments.ocr_language);
    }
    apply_ai_provider_overrides(arguments, loaded)
}

fn apply_asr_overrides(
    arguments: &ConversionArgs,
    options: &mut ConversionOptions,
) -> Result<(), CliError> {
    if let Some(language) = &arguments.asr_language {
        options.asr.language = Some(language.clone());
    }
    if let Some(script) = arguments.chinese_script {
        options.asr.chinese_script = match script {
            crate::args::ChineseScriptArg::Preserve => into_markdown::ChineseScript::Preserve,
            crate::args::ChineseScriptArg::Simplified => into_markdown::ChineseScript::Simplified,
            crate::args::ChineseScriptArg::Traditional => into_markdown::ChineseScript::Traditional,
        };
    }
    if let Some(threads) = arguments.asr_threads {
        options.asr.max_threads = threads;
    }
    if let Some(duration) = arguments.asr_max_duration_ms {
        options.asr.max_duration_ms = Some(duration);
    }
    if arguments.diarize {
        options.diarization.enabled = true;
        options.diarization.expected_speakers = arguments.expected_speakers;
        if let Some(expected) = arguments.expected_speakers {
            options.diarization.max_speakers = options.diarization.max_speakers.max(expected);
        }
        options.ai.audio_transcription = AiMode::Only;
    }
    let asr = &options.asr;
    let language_valid = asr.language.as_deref().is_none_or(|language| {
        !language.is_empty()
            && language.len() <= 35
            && language.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    if !(1..=8).contains(&asr.max_threads)
        || asr.max_duration_ms == Some(0)
        || !(1..=100_000).contains(&asr.max_segments)
        || !(256 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&asr.max_native_memory_bytes)
        || !language_valid
    {
        return Err(CliError::from(into_markdown::ConversionError::ResourceLimit {
            limit: "asrConfiguration",
            detail: "ASR options exceed the supported local provider envelope".into(),
        }));
    }
    Ok(())
}

fn apply_ocr_overrides(
    arguments: &ConversionArgs,
    options: &mut ConversionOptions,
) -> Result<(), CliError> {
    if let Some(policy) = arguments.ocr {
        options.ocr.policy = match policy {
            OcrPolicyArg::Off => OcrPolicy::Off,
            OcrPolicyArg::Auto => OcrPolicy::Auto,
            OcrPolicyArg::Always => OcrPolicy::Always,
        };
    }
    if let Some(confidence) = arguments.ocr_min_confidence {
        config::validate_confidence(confidence)?;
        options.ocr.minimum_confidence = confidence;
    }
    Ok(())
}

fn apply_text_overrides(arguments: &ConversionArgs, options: &mut ConversionOptions) {
    if let Some(mode) = arguments.encoding_errors {
        options.text.decoding_mode = match mode {
            EncodingErrorsArg::Strict => TextDecodingMode::Strict,
            EncodingErrorsArg::Replace => TextDecodingMode::Replace,
        };
    }
    apply_delimited_overrides(arguments, options);
}

fn apply_network_overrides(
    arguments: &ConversionArgs,
    options: &mut ConversionOptions,
) -> Result<(), CliError> {
    if let Some(value) = arguments.max_redirects {
        options.network.max_redirects = value;
    }
    apply_network_authorization(
        options,
        arguments.allow_network,
        arguments.allow_private_network,
        &arguments.allow_host,
    )
}

fn apply_output_overrides(arguments: &ConversionArgs, options: &mut ConversionOptions) {
    if let Some(mode) = arguments.asset_mode {
        options.output.asset_mode = match mode {
            AssetModeArg::Extract => AssetMode::Extract,
            AssetModeArg::Embed => AssetMode::Embed,
            AssetModeArg::Omit => AssetMode::Omit,
        };
    }
}

fn apply_ai_capability_overrides(
    arguments: &ConversionArgs,
    loaded: &mut LoadedConfig,
) -> Result<(), CliError> {
    for assignment in &arguments.ai {
        let (capability, mode) = split_assignment(assignment, "--ai")?;
        config::validate_capability(capability)?;
        let mode = parse_ai_mode(mode)?;
        set_ai_mode(&mut loaded.options, capability, mode);
    }
    for prompt in &arguments.ai_prompt {
        let (capability, path) = split_assignment(prompt, "--ai-prompt")?;
        config::validate_capability(capability)?;
        if !Path::new(path).is_file() {
            return Err(CliError::usage(format!("AI prompt file does not exist: {path}")));
        }
        loaded.prompts.insert(capability.into(), PathBuf::from(path));
    }
    Ok(())
}

fn apply_ai_provider_overrides(
    arguments: &ConversionArgs,
    loaded: &mut LoadedConfig,
) -> Result<(), CliError> {
    if let Some(provider) = &arguments.ai_provider {
        if !loaded.effective.providers.contains_key(provider) {
            return Err(CliError::usage(format!("unknown AI provider '{provider}'")));
        }
        loaded.ai_provider = Some(provider.clone());
    }
    if let Some(model) = &arguments.ai_model {
        loaded.ai_model = Some(model.clone());
    }
    if provider_backed_ai_capability_is_enabled(&loaded.options) {
        let Some(provider_name) = loaded.ai_provider.as_deref() else {
            return Err(CliError::usage(
                "an enabled AI capability requires --ai-provider or a configured default \
                 provider",
            ));
        };
        validate_configured_provider_network(loaded, provider_name)?;
    }
    for route in
        [&loaded.effective.capability_routes.ocr, &loaded.effective.capability_routes.transcription]
    {
        for source in route.primary.iter().chain(&route.fallbacks) {
            let Ok(into_markdown_provider_plugin::CapabilitySourceRef::Provider {
                provider_id,
                ..
            }) = source.parse()
            else {
                continue;
            };
            validate_configured_provider_network(loaded, &provider_id)?;
        }
    }
    Ok(())
}

fn validate_configured_provider_network(
    loaded: &LoadedConfig,
    provider_name: &str,
) -> Result<(), CliError> {
    let provider = loaded
        .effective
        .providers
        .get(provider_name)
        .ok_or_else(|| CliError::usage(format!("unknown AI provider '{provider_name}'")))?;
    validate_network_url(&provider.base_url, &loaded.options, "AI provider")
}

fn provider_backed_ai_capability_is_enabled(options: &ConversionOptions) -> bool {
    [
        options.ai.image_description,
        options.ai.layout_repair,
        options.ai.table_repair,
        options.ai.formula_repair,
        options.ai.markdown_postprocess,
    ]
    .iter()
    .any(|mode| *mode != AiMode::Off)
}

fn apply_delimited_overrides(arguments: &ConversionArgs, options: &mut ConversionOptions) {
    if let Some(mode) = arguments.table_header {
        options.delimited_text.header = match mode {
            TableHeaderArg::Auto => TableHeaderMode::Auto,
            TableHeaderArg::Always => TableHeaderMode::Always,
            TableHeaderArg::Never => TableHeaderMode::Never,
        };
    }
    if let Some(mode) = arguments.ragged_rows {
        options.delimited_text.ragged_rows = match mode {
            RaggedRowsArg::Strict => RaggedRowsMode::Strict,
            RaggedRowsArg::Pad => RaggedRowsMode::Pad,
        };
    }
}

fn set_ai_mode(options: &mut ConversionOptions, capability: &str, mode: AiMode) {
    match capability {
        "vision-ocr" => options.ai.vision_ocr = mode,
        "image-description" => options.ai.image_description = mode,
        "layout-repair" => options.ai.layout_repair = mode,
        "table-repair" => options.ai.table_repair = mode,
        "formula-repair" => options.ai.formula_repair = mode,
        "audio-transcription" => options.ai.audio_transcription = mode,
        "markdown-postprocess" => options.ai.markdown_postprocess = mode,
        _ => {}
    }
}

fn parse_ai_mode(value: &str) -> Result<AiMode, CliError> {
    match value {
        "off" => Ok(AiMode::Off),
        "fallback" => Ok(AiMode::Fallback),
        "prefer" => Ok(AiMode::Prefer),
        "only" => Ok(AiMode::Only),
        _ => Err(CliError::usage(format!("unknown AI mode '{value}'"))),
    }
}

fn split_assignment<'a>(value: &'a str, option: &str) -> Result<(&'a str, &'a str), CliError> {
    value
        .split_once('=')
        .filter(|(left, right)| !left.is_empty() && !right.is_empty())
        .ok_or_else(|| CliError::usage(format!("{option} expects NAME=VALUE")))
}

fn expand_inputs(
    arguments: &ConversionArgs,
    includes: Option<&GlobSet>,
    excludes: Option<&GlobSet>,
) -> Result<Vec<WorkItem>, CliError> {
    let stdin_count = arguments.inputs.iter().filter(|value| value.as_os_str() == "-").count();
    if stdin_count > 0 && arguments.inputs.len() != 1 {
        return Err(CliError::usage("standard input '-' cannot be combined with other inputs"));
    }
    let mut output = Vec::new();
    for value in &arguments.inputs {
        if value.as_os_str() == "-" {
            output.push(WorkItem {
                input: InputRef::Stdin,
                display: "stdin".into(),
                relative: PathBuf::from("stdin"),
                root_label: "stdin".into(),
                input_root: PathBuf::from("stdin:"),
                from_directory: false,
                local_path: None,
            });
            continue;
        }
        let text = value.to_string_lossy();
        if is_uri(&text) {
            let parsed = url::Url::parse(&text)
                .map_err(|error| CliError::usage(format!("invalid input URI: {error}")))?;
            let name = parsed
                .path_segments()
                .and_then(Iterator::last)
                .filter(|name| !name.is_empty())
                .unwrap_or("remote-document");
            output.push(WorkItem {
                input: InputRef::Uri(text.into_owned()),
                display: redact_parsed_url(parsed.clone()),
                relative: PathBuf::from(sanitize_component(name)),
                root_label: sanitize_component(parsed.host_str().unwrap_or("remote")),
                input_root: PathBuf::from(redact_parsed_url(parsed)),
                from_directory: false,
                local_path: None,
            });
            continue;
        }
        let path = PathBuf::from(value);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(CliError::new(
                ExitClass::Policy,
                "symlinkDenied",
                format!("input symlinks are not followed: {}", path.display()),
            ));
        }
        if metadata.is_dir() {
            if !arguments.recursive {
                return Err(CliError::usage(format!(
                    "directory input requires --recursive: {}",
                    path.display()
                )));
            }
            let root_label = path
                .file_name()
                .and_then(OsStr::to_str)
                .map_or_else(|| "root".into(), sanitize_component);
            walk_directory(
                &path,
                &path,
                &root_label,
                arguments.hidden,
                includes,
                excludes,
                &mut output,
            )?;
        } else if metadata.is_file() {
            let relative =
                path.file_name().map_or_else(|| PathBuf::from("document"), PathBuf::from);
            output.push(WorkItem {
                input: InputRef::Path(path.clone()),
                display: path.display().to_string(),
                relative,
                root_label: path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(OsStr::to_str)
                    .map_or_else(|| "input".into(), sanitize_component),
                input_root: path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(),
                from_directory: false,
                local_path: Some(path),
            });
        }
    }
    Ok(output)
}

fn walk_directory(
    root: &Path,
    directory: &Path,
    root_label: &str,
    include_hidden: bool,
    includes: Option<&GlobSet>,
    excludes: Option<&GlobSet>,
    output: &mut Vec<WorkItem>,
) -> Result<(), CliError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| CliError::internal(format!("derive relative input path: {error}")))?;
        if !include_hidden
            && relative.components().any(|component| is_hidden(component.as_os_str()))
        {
            continue;
        }
        if excludes.is_some_and(|set| set.is_match(relative)) {
            continue;
        }
        if file_type.is_dir() {
            walk_directory(root, &path, root_label, include_hidden, includes, excludes, output)?;
        } else if file_type.is_file() && includes.is_none_or(|set| set.is_match(relative)) {
            output.push(WorkItem {
                input: InputRef::Path(path.clone()),
                display: path.display().to_string(),
                relative: relative.to_path_buf(),
                root_label: root_label.to_owned(),
                input_root: root.to_path_buf(),
                from_directory: true,
                local_path: Some(path),
            });
        }
    }
    Ok(())
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, CliError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern)
                .map_err(|error| CliError::usage(format!("invalid glob '{pattern}': {error}")))?,
        );
    }
    builder.build().map(Some).map_err(|error| CliError::usage(format!("invalid glob set: {error}")))
}

fn plan_outputs(
    items: Vec<WorkItem>,
    output: Option<&Path>,
    output_dir: Option<&Path>,
    emit: EmitKind,
) -> Vec<WorkPlan> {
    items
        .into_iter()
        .map(|item| {
            let destination = output.map(Path::to_path_buf).or_else(|| {
                output_dir.map(|directory| {
                    let mut relative = item.relative.clone();
                    let stem =
                        relative.file_stem().and_then(|value| value.to_str()).unwrap_or("document");
                    relative.set_file_name(format!("{stem}.{}", emit.extension()));
                    directory.join(relative)
                })
            });
            WorkPlan { item, output: destination, output_root: output_dir.map(Path::to_path_buf) }
        })
        .collect()
}

fn disambiguate_planned_outputs(plans: &mut [WorkPlan]) {
    let mut collisions = BTreeMap::<PathBuf, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate() {
        if let Some(path) = &plan.output {
            collisions.entry(path.clone()).or_default().push(index);
        }
    }
    for indexes in collisions.values().filter(|indexes| indexes.len() > 1) {
        let distinct_roots = indexes
            .iter()
            .map(|index| plans[*index].item.input_root.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for index in indexes {
            let plan = &mut plans[*index];
            let Some(path) = plan.output.as_mut() else { continue };
            if distinct_roots.len() > 1 {
                if let Some(root) = &plan.output_root
                    && let Ok(relative) = path.strip_prefix(root)
                {
                    *path = root.join(&plan.item.root_label).join(relative);
                }
            } else {
                *path = qualify_output_with_source_name(path, &plan.item.relative);
            }
        }
    }
    let mut seen = BTreeMap::<PathBuf, usize>::new();
    for plan in plans {
        let Some(path) = plan.output.as_mut() else {
            continue;
        };
        let count = seen.entry(path.clone()).or_default();
        if *count > 0 {
            *path = add_numeric_suffix(path, *count);
        }
        *count += 1;
    }
}

fn qualify_output_with_source_name(output: &Path, source: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let source_name = source.file_name().and_then(OsStr::to_str).unwrap_or("document");
    let output_name = output.file_name().and_then(OsStr::to_str).unwrap_or("document.md");
    let suffix = if output_name.ends_with(".mdpkg.zip") {
        "mdpkg.zip"
    } else {
        output.extension().and_then(OsStr::to_str).unwrap_or("md")
    };
    parent.join(format!("{source_name}.{suffix}"))
}

fn add_numeric_suffix(path: &Path, number: usize) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().and_then(|value| value.to_str()).unwrap_or("document");
    if let Some(stem) = filename.strip_suffix(".mdpkg.zip") {
        parent.join(format!("{stem}-{number}.mdpkg.zip"))
    } else {
        let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or("document");
        path.extension().and_then(|value| value.to_str()).map_or_else(
            || parent.join(format!("{stem}-{number}")),
            |extension| parent.join(format!("{stem}-{number}.{extension}")),
        )
    }
}

fn process_stdout(
    plan: &WorkPlan,
    policy: &ExecutionPolicy,
    _catalog: Catalog,
    _json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<BatchItemReport, CliError> {
    let asset_output = plan_stdout_asset_output(
        policy.emit,
        policy.asset_mode,
        policy.assets_dir.as_deref(),
        plan.item.local_path.as_deref(),
        &policy.working_directory,
    )?;
    let (mut spool, summary) = convert_item_into(&plan.item, policy, asset_output.uri_prefix)?;
    let processing_duration_ms = summary.processing_duration_ms;
    (|| -> Result<BatchItemReport, CliError> {
        crate::result_policy::validate_for_emit(&summary, policy.emit, policy.asset_mode)?;
        spool.finish()?;
        let mut encoded =
            policy.output_context.temporary_file("into-md-stdout").map_err(CliError::from)?;
        spool.serialize(policy.emit, &mut encoded)?;
        encoded.sync_all().map_err(CliError::from)?;
        let staged_assets = if let Some(assets_dir) = asset_output.external_directory {
            spool
                .has_payloads()
                .then(|| {
                    output::stage_spooled_assets(
                        &spool,
                        &assets_dir,
                        policy.asset_mode,
                        policy.conflict,
                        &policy.output_context,
                    )
                })
                .transpose()?
        } else if policy.asset_mode == AssetModeArg::Extract
            && policy.emit != EmitKind::Bundle
            && spool.has_payloads()
        {
            return Err(CliError::usage(
                "stdin and URI inputs with extracted assets require --assets-dir",
            ));
        } else {
            None
        };
        output::publish_stdout(&encoded, context.stdout, staged_assets, &policy.output_context)?;
        Ok(BatchItemReport {
            input: plan.item.display.clone(),
            output: None,
            format: summary.format.map(|format| format.as_str().into()),
            status: BatchItemStatus::Success,
            outcome: crate::result_policy::batch_outcome(summary.outcome),
            diagnostics: summary.diagnostics.iter().map(Into::into).collect(),
            error_code: None,
            reason_code: summary.reason_code().map(str::to_owned),
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: None,
            processing_duration_ms,
        })
    })()
    .map_err(|error| error.with_processing_duration(processing_duration_ms))
}

fn process_batch(
    plans: Vec<WorkPlan>,
    policy: &ExecutionPolicy,
    jobs: usize,
) -> Result<Vec<BatchItemReport>, CliError> {
    let task_count = plans.len();
    let queue = Arc::new(Mutex::new(VecDeque::from(plans)));
    // Conversions may run in parallel, but the atomic writer intentionally
    // leases an output parent directory. Keep only the short preflight and
    // commit phases serialized so sibling outputs cannot trip each other's
    // authenticated parent lease.
    let output_phase = Arc::new(Mutex::new(()));
    // Converter preflight credits intentionally cover each converter's full
    // request envelope. Hold one batch-wide admission lease until that item's
    // retained result has been committed so `--jobs` can never multiply the
    // shared memory ceiling.
    let memory_phase = Arc::new(Mutex::new(()));
    let (sender, receiver) = mpsc::channel();
    let worker_count = jobs.max(1).min(task_count.max(1));
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let output_phase = Arc::clone(&output_phase);
            let memory_phase = Arc::clone(&memory_phase);
            let sender = sender.clone();
            let policy = policy.clone();
            scope.spawn(move || {
                loop {
                    let task = match queue.lock() {
                        Ok(mut queue) => queue.pop_front(),
                        Err(_) => None,
                    };
                    let Some(task) = task else {
                        break;
                    };
                    let index_key = task.item.display.clone();
                    let report = process_file_task(task, &policy, &output_phase, &memory_phase);
                    if sender.send((index_key, report)).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    let mut reports = receiver.into_iter().collect::<Vec<_>>();
    reports.sort_by(|left, right| left.0.cmp(&right.0));
    if reports.len() != task_count {
        return Err(CliError::internal("batch scheduler did not return every input"));
    }
    Ok(reports.into_iter().map(|(_, report)| report).collect())
}

fn process_file_task(
    plan: WorkPlan,
    policy: &ExecutionPolicy,
    output_phase: &Mutex<()>,
    memory_phase: &Mutex<()>,
) -> BatchItemReport {
    let _memory_guard = memory_phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let item_timer = crate::timing::ItemTimer::start();
    match process_file_task_inner(&plan, policy, output_phase) {
        Ok(completed) => {
            completed_item_report(plan.item.display, completed, item_timer.elapsed_ms())
        }
        Err(error) => failed_item_report(&plan, policy, &error, Some(item_timer.elapsed_ms())),
    }
}

fn completed_item_report(
    input: String,
    completed: crate::result_policy::CommittedOutput,
    duration_ms: f64,
) -> BatchItemReport {
    let reason_code = completed.summary.reason_code().map(str::to_owned);
    BatchItemReport {
        input,
        output: Some(report_path(&completed.path)),
        format: completed.summary.format.map(|format| format.as_str().into()),
        status: BatchItemStatus::Success,
        outcome: crate::result_policy::batch_outcome(completed.summary.outcome),
        diagnostics: completed.summary.diagnostics.iter().map(Into::into).collect(),
        error_code: None,
        reason_code,
        component: None,
        part: None,
        limit: None,
        message: None,
        warnings: completed.warnings,
        duration_ms: Some(duration_ms),
        processing_duration_ms: completed.summary.processing_duration_ms,
    }
}

fn failed_item_report(
    plan: &WorkPlan,
    policy: &ExecutionPolicy,
    error: &CliError,
    duration_ms: Option<f64>,
) -> BatchItemReport {
    BatchItemReport {
        input: plan.item.display.clone(),
        output: plan.output.as_deref().map(report_path),
        format: error.detected_format().or(policy.hint.format).map(|format| format.as_str().into()),
        status: BatchItemStatus::Failed,
        outcome: BatchItemOutcome::Failed,
        diagnostics: vec![],
        error_code: Some(error.code().into()),
        reason_code: Some(error.reason_code().into()),
        component: error.component_name().map(str::to_owned),
        part: error.part().map(str::to_owned),
        limit: error
            .limit()
            .map(|(name, detail)| BatchLimitDto { name: name.into(), detail: Some(detail.into()) }),
        message: Some(error.to_string()),
        warnings: vec![],
        duration_ms,
        processing_duration_ms: error.processing_duration_ms(),
    }
}

fn process_file_task_inner(
    plan: &WorkPlan,
    policy: &ExecutionPolicy,
    output_phase: &Mutex<()>,
) -> Result<crate::result_policy::CommittedOutput, CliError> {
    let requested =
        plan.output.as_deref().ok_or_else(|| CliError::internal("batch output path is absent"))?;
    let output_path = {
        let _guard =
            output_phase.lock().map_err(|_| CliError::internal("batch output lock is poisoned"))?;
        output::preflight_file(requested, policy.conflict, &policy.output_context)?
    };
    let asset_output = plan_file_asset_output(
        policy.emit,
        policy.asset_mode,
        policy.assets_dir.as_deref(),
        &output_path,
        &policy.working_directory,
    )?;
    let (mut spool, summary) = convert_item_into(&plan.item, policy, asset_output.uri_prefix)?;
    let processing_duration_ms = summary.processing_duration_ms;
    (|| -> Result<crate::result_policy::CommittedOutput, CliError> {
        crate::result_policy::validate_for_emit(&summary, policy.emit, policy.asset_mode)?;
        spool.finish()?;
        let output_parent = output_path
            .parent()
            .ok_or_else(|| CliError::internal("batch output has no parent directory"))?;
        let mut encoded = policy
            .output_context
            .temporary_file_in(output_parent, "into-md-encoded")
            .map_err(CliError::from)?;
        spool.serialize(policy.emit, &mut encoded)?;
        encoded.sync_all().map_err(CliError::from)?;
        let write_outcome = {
            let _guard = output_phase
                .lock()
                .map_err(|_| CliError::internal("batch output lock is poisoned"))?;
            output::write_spooled_output_set_file(
                &output_path,
                encoded.as_file().map_err(CliError::from)?,
                &spool,
                asset_output.external_directory.as_deref(),
                policy.asset_mode,
                policy.conflict,
                &policy.output_context,
            )?
        };
        let mut warnings = Vec::new();
        if write_outcome.renamed || output_path != requested {
            warnings.push(format!(
                "output renamed to {} because the requested path existed",
                write_outcome.path.display()
            ));
        }
        Ok(crate::result_policy::CommittedOutput { path: write_outcome.path, summary, warnings })
    })()
    .map_err(|error| error.with_processing_duration(processing_duration_ms))
}

fn report_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn convert_item_into(
    item: &WorkItem,
    policy: &ExecutionPolicy,
    asset_uri_prefix: Option<String>,
) -> Result<(output::StructuredSpool, ConversionSummary), CliError> {
    validate_input_network(&item.input, &policy.options)?;
    let mut request = ConversionRequest::new(item.input.clone());
    request.options = policy.options.clone();
    if policy.asset_mode == AssetModeArg::Extract {
        request.options.output.asset_uri_prefix = asset_uri_prefix;
    }
    request.execution = policy.execution.clone();
    request.hint = policy.hint.clone();
    let engine = into_markdown::default_engine_with_services(policy.services.clone())
        .map_err(CliError::from)?;
    let context = policy.output_context.fork_with_shared_resources(policy.execution.clone());
    let observed_context = context.clone();
    let mut spool = output::StructuredSpool::new(context.clone(), policy.emit, policy.asset_mode)?;
    let result = futures::executor::block_on(async {
        let prepared =
            engine.prepare_into_with_context(request, context, spool.capabilities()).await?;
        engine.execute_prepared_into(prepared, &mut spool).await
    })
    .map_err(|error| {
        CliError::from(error).with_detected_format(observed_context.detected_format())
    })?;
    Ok((spool, result))
}

fn plan_stdout_asset_output(
    emit: EmitKind,
    mode: AssetModeArg,
    configured_directory: Option<&Path>,
    local_input: Option<&Path>,
    working_directory: &Path,
) -> Result<AssetOutputPlan, CliError> {
    if mode != AssetModeArg::Extract {
        return Ok(AssetOutputPlan { uri_prefix: None, external_directory: None });
    }
    if emit == EmitKind::Bundle {
        return Ok(AssetOutputPlan { uri_prefix: Some("assets".into()), external_directory: None });
    }
    let external_directory = configured_directory.map(Path::to_path_buf).or_else(|| {
        local_input.map(|input| default_stdout_asset_directory(input, working_directory))
    });
    let uri_prefix = external_directory
        .as_deref()
        .map(|directory| asset_uri_prefix_for_stdout(directory, working_directory))
        .transpose()?;
    Ok(AssetOutputPlan { uri_prefix, external_directory })
}

fn plan_file_asset_output(
    emit: EmitKind,
    mode: AssetModeArg,
    configured_directory: Option<&Path>,
    output: &Path,
    working_directory: &Path,
) -> Result<AssetOutputPlan, CliError> {
    if mode != AssetModeArg::Extract {
        return Ok(AssetOutputPlan { uri_prefix: None, external_directory: None });
    }
    if emit == EmitKind::Bundle {
        return Ok(AssetOutputPlan { uri_prefix: Some("assets".into()), external_directory: None });
    }
    let external_directory =
        configured_directory.map_or_else(|| default_asset_directory(output), Path::to_path_buf);
    Ok(AssetOutputPlan {
        uri_prefix: Some(asset_uri_prefix_for_file(
            output,
            &external_directory,
            working_directory,
        )?),
        external_directory: Some(external_directory),
    })
}

fn validate_input_network(input: &InputRef, options: &ConversionOptions) -> Result<(), CliError> {
    let InputRef::Uri(value) = input else {
        return Ok(());
    };
    validate_network_url(value, options, "URI input")
}

fn validate_network_url(
    value: &str,
    options: &ConversionOptions,
    target: &str,
) -> Result<(), CliError> {
    if !options.network.enabled {
        return Err(CliError::new(
            ExitClass::Policy,
            "networkDenied",
            format!("{target} requires --allow-network"),
        ));
    }
    let parsed = url::Url::parse(value)
        .map_err(|error| CliError::usage(format!("invalid {target} URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CliError::new(
            ExitClass::Policy,
            "networkUrlDenied",
            format!("{target} URL must use http or https"),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CliError::new(
            ExitClass::Policy,
            "networkUrlDenied",
            format!("{target} URL must not include user information"),
        ));
    }
    let host = parsed.host_str().ok_or_else(|| {
        CliError::new(
            ExitClass::Policy,
            "networkUrlDenied",
            format!("{target} URL must include a hostname"),
        )
    })?;
    let normalized_host = config::normalize_allowed_host(host)?;
    let allowed_hosts = config::normalize_allowed_hosts(&options.network.allowed_hosts)?;
    if !allowed_hosts.is_empty() && !allowed_hosts.iter().any(|allowed| allowed == &normalized_host)
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "hostDenied",
            format!("{target} hostname '{host}' is not in the effective host allowlist"),
        ));
    }
    if options.network.deny_private_networks && is_obviously_private_host(&normalized_host) {
        return Err(CliError::new(
            ExitClass::Policy,
            "privateNetworkDenied",
            format!("{target} hostname '{host}' requires --allow-private-network"),
        ));
    }
    Ok(())
}

fn apply_network_authorization(
    options: &mut ConversionOptions,
    allow_network: bool,
    allow_private_network: bool,
    allow_hosts: &[String],
) -> Result<(), CliError> {
    options.network.enabled = allow_network;
    options.network.deny_private_networks = !allow_private_network;
    narrow_allowed_hosts(&mut options.network.allowed_hosts, allow_hosts)
}

fn narrow_allowed_hosts(
    configured: &mut Vec<String>,
    requested: &[String],
) -> Result<(), CliError> {
    let configured_hosts = config::normalize_allowed_hosts(configured)?;
    let requested_hosts = config::normalize_allowed_hosts(requested)?;
    *configured = match (configured_hosts.is_empty(), requested_hosts.is_empty()) {
        (true, true) => Vec::new(),
        (false, true) => configured_hosts,
        (true, false) => requested_hosts,
        (false, false) => {
            let requested = requested_hosts.into_iter().collect::<std::collections::BTreeSet<_>>();
            let intersection = configured_hosts
                .into_iter()
                .filter(|host| requested.contains(host))
                .collect::<Vec<_>>();
            if intersection.is_empty() {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "hostAllowlistConflict",
                    "--allow-host does not overlap conversion.network.allowed_hosts; networking remains denied",
                ));
            }
            intersection
        }
    };
    Ok(())
}

fn is_obviously_private_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    let host = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
    host.parse::<std::net::IpAddr>().is_ok_and(|address| match address {
        std::net::IpAddr::V4(address) => is_private_ipv4(address),
        std::net::IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                is_private_ipv4(mapped)
            } else {
                let first = address.segments()[0];
                address.is_loopback()
                    || address.is_unspecified()
                    || address.is_multicast()
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80
            }
        }
    })
}

fn is_private_ipv4(address: std::net::Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
}

fn default_asset_directory(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or("document");
    parent.join(format!("{stem}_assets"))
}

fn default_stdout_asset_directory(input: &Path, working_directory: &Path) -> PathBuf {
    let stem = input.file_stem().and_then(|value| value.to_str()).unwrap_or("document");
    working_directory.join(format!("{stem}_assets"))
}

fn parse_input(value: &OsStr) -> Result<InputRef, CliError> {
    if value == "-" {
        Ok(InputRef::Stdin)
    } else {
        let text = value.to_string_lossy();
        if is_uri(&text) {
            url::Url::parse(&text)
                .map_err(|error| CliError::usage(format!("invalid input URI: {error}")))?;
            Ok(InputRef::Uri(text.into_owned()))
        } else {
            Ok(InputRef::Path(PathBuf::from(value)))
        }
    }
}

fn is_uri(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn is_hidden(value: &OsStr) -> bool {
    value.to_str().is_some_and(|value| value.starts_with('.') && value != "." && value != "..")
}

fn sanitize_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() { "document".into() } else { value }
}

fn parse_format(value: &str) -> Result<InputFormat, CliError> {
    find_format(value)
        .map(|entry| entry.descriptor.format)
        .ok_or_else(|| CliError::usage(format!("unknown format '{value}'")))
}

fn find_format(value: &str) -> Option<&'static into_markdown::CatalogFormatDescriptor> {
    let normalized = value.trim_start_matches('.').to_ascii_lowercase();
    into_markdown::format_catalog().iter().find(|entry| {
        entry.descriptor.format.as_str() == normalized
            || entry.descriptor.extensions.iter().any(|extension| *extension == normalized)
    })
}

fn write_json(stdout: &mut dyn Write, value: &impl Serialize) -> Result<(), CliError> {
    serde_json::to_writer_pretty(&mut *stdout, value)
        .map_err(|error| CliError::internal(format!("serialize JSON: {error}")))?;
    writeln!(stdout)?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StderrEvent<'a> {
    level: &'a str,
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a str>,
}

fn write_stderr_event(
    stderr: &mut dyn Write,
    json_log: bool,
    level: &str,
    code: &str,
    message: &str,
    input: Option<&str>,
    text: &str,
) -> Result<(), CliError> {
    if json_log {
        serde_json::to_writer(&mut *stderr, &StderrEvent { level, code, message, input })
            .map_err(|error| CliError::internal(format!("serialize stderr event: {error}")))?;
        writeln!(stderr)?;
    } else {
        writeln!(stderr, "{text}")?;
    }
    Ok(())
}

fn redact_url(value: &str) -> String {
    if let Ok(parsed) = url::Url::parse(value) {
        redact_parsed_url(parsed)
    } else {
        "<invalid-url>".into()
    }
}

fn redact_parsed_url(mut parsed: url::Url) -> String {
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

#[cfg(test)]
#[path = "app/timing_tests.rs"]
mod timing_tests;

#[cfg(test)]
#[path = "app/resource_usage_tests.rs"]
mod resource_usage_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::transaction::{HookDecision, Target};
    use into_markdown::{
        Asset, AssetId, Block, BlockNode, Document, NodeId, Provenance, ProvenanceKind,
        SourceLocator, render_markdown,
    };
    use pulldown_cmark::{Event, Parser as MarkdownParser, Tag};
    use sha2::{Digest, Sha256};
    use std::io::Cursor;

    #[test]
    fn stdout_and_batch_outcomes_preserve_authoritative_conversion_semantics() {
        let informational_diagnostics = [into_markdown::Diagnostic {
            code: "informational".into(),
            severity: into_markdown::DiagnosticSeverity::Info,
            message: "lossless note".into(),
            locator: None,
        }];
        let degrading_diagnostics = [into_markdown::Diagnostic {
            code: "contentOmitted".into(),
            severity: into_markdown::DiagnosticSeverity::Warning,
            message: "content was omitted".into(),
            locator: None,
        }];
        assert_eq!(
            crate::result_policy::batch_outcome(into_markdown::conversion_outcome(
                &informational_diagnostics,
            )),
            BatchItemOutcome::Complete,
            "stdout and batch must not infer degradation from informational diagnostics"
        );
        assert_eq!(
            crate::result_policy::batch_outcome(into_markdown::conversion_outcome(
                &degrading_diagnostics,
            )),
            BatchItemOutcome::Degraded,
            "recoverable content loss must remain degraded"
        );
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

    #[test]
    fn plugin_lifecycle_budget_covers_package_and_extracted_tree() {
        let execution = plugin_lifecycle_execution_context();
        assert_eq!(
            execution.resource_limits().max_temporary_bytes,
            MAX_PLUGIN_LIFECYCLE_TEMPORARY_BYTES
        );
        const { assert!(MAX_PLUGIN_LIFECYCLE_TEMPORARY_BYTES >= 5 * MAX_PLUGIN_PACKAGE_BYTES) };
    }

    #[test]
    fn plugin_test_authority_never_falls_back_to_real_user_data() {
        let error = global_plugin_store_scope().unwrap_err();
        assert_eq!(error.code(), "internal");
    }

    #[cfg(unix)]
    #[test]
    fn plugin_user_data_parent_rejects_world_writable_authority() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let anchor = temporary.path().join("user-data");
        fs::create_dir(&anchor).unwrap();
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o777)).unwrap();
        let _test_user_data = TestUserDataGuard::set(Some(anchor.clone()));

        let error = global_plugin_store_scope().unwrap_err();
        assert_eq!(error.code(), "config");
        assert!(!anchor.join("into-markdown").exists());
    }

    fn signed_transaction_fixture() -> (Vec<u8>, String, String) {
        use base64::Engine as _;
        use into_markdown_plugin_manager::{PackageFile, PackageManifest, PackageSignature};
        use ring::signature::{Ed25519KeyPair, KeyPair as _};
        use zip::write::SimpleFileOptions;

        let key = Ed25519KeyPair::from_seed_unchecked(&[9_u8; 32]).unwrap();
        let public = key.public_key().as_ref();
        let fingerprint = format!("{:x}", Sha256::digest(public));
        #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
        let target = "x86_64-pc-windows-msvc".to_owned();
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        let target = "x86_64-unknown-linux-gnu".to_owned();
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        let target = "aarch64-unknown-linux-gnu".to_owned();
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        let target = "aarch64-apple-darwin".to_owned();
        let contents = b"fixture";
        let mut manifest = PackageManifest {
            schema_version: 1,
            id: "transaction-fixture".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: "process-v1".to_owned(),
            supported_targets: std::collections::BTreeSet::from([target.clone()]),
            entrypoints: BTreeMap::from([(target, "fixture.exe".to_owned())]),
            runtime_manifest: None,
            files: vec![PackageFile {
                path: "fixture.exe".to_owned(),
                bytes: contents.len() as u64,
                sha256: format!("{:x}", Sha256::digest(contents)),
                executable: true,
            }],
            signature: PackageSignature {
                signed_payload_version: 1,
                algorithm: "ed25519".to_owned(),
                key_id: "publisher.transaction".to_owned(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
                public_key_sha256: fingerprint.clone(),
                signed_payload_sha256: String::new(),
                signature_base64: String::new(),
            },
        };
        let payload = into_markdown_plugin_manager::canonical_signed_payload(&manifest).unwrap();
        manifest.signature.signed_payload_sha256 = format!("{:x}", Sha256::digest(&payload));
        manifest.signature.signature_base64 =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("plugin.json", options).unwrap();
        writer.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        writer.start_file("fixture.exe", options).unwrap();
        writer.write_all(contents).unwrap();
        (writer.finish().unwrap().into_inner(), "publisher.transaction".to_owned(), fingerprint)
    }

    #[test]
    fn joint_transaction_config_cas_crash_child() {
        let Some(root) = std::env::var_os("INTO_MD_JOINT_TXN_ROOT") else { return };
        let mode = std::env::var("INTO_MD_JOINT_TXN_MODE").unwrap();
        let root = PathBuf::from(root);
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let user_data = root.join("appdata");
        create_private_user_data_directory(&user_data).unwrap();
        let _test_user_data = TestUserDataGuard::set(Some(user_data));
        let (anchor, relative) = global_plugin_store_scope().unwrap();
        let (package, key_id, fingerprint) = signed_transaction_fixture();
        let mut global = if mode == "seed" {
            PluginManager::open_scoped(&anchor, &relative, Default::default()).unwrap()
        } else {
            PluginManager::open_persisted_scoped(&anchor, &relative).unwrap()
        };
        if mode == "seed" {
            global.trust_signer(&key_id, &fingerprint).unwrap();
            let (_, project_relative) = project_plugin_store_scope(&project).unwrap();
            let manager =
                PluginManager::open_scoped(&anchor, &project_relative, global.trusted_signers())
                    .unwrap();
            let execution = into_markdown::ExecutionContext::new(
                into_markdown::ExecutionOptions::default(),
                into_markdown::ResourceLimits::default(),
            );
            let installed = manager.install_bytes(&package, None, &execution).unwrap();
            let configured = PluginConfig {
                source: "fixture.zip".to_owned(),
                sha256: Some(installed.package_sha256),
                protocol: installed.protocol,
                enabled: true,
                signing_key_id: key_id.clone(),
                signing_key_sha256: fingerprint.clone(),
            };
            let config_path = config::scope_path(crate::args::Scope::Project, &project).unwrap();
            let config_lock = config::lock_exact(&config_path).unwrap();
            config::compare_and_set_plugin_exact_locked(
                &config_lock,
                &config_path,
                &installed.id,
                None,
                Some(&configured),
            )
            .unwrap();
            let transaction = CliPluginTransaction {
                schema_version: 1,
                operation: CliPluginOperation::Install,
                phase: CliPluginPhase::StoreChanged,
                global: false,
                store_relative: project_relative.to_string_lossy().into_owned(),
                project_root: Some(project.canonicalize().unwrap()),
                id: installed.id,
                backup_name: None,
                old_config: None,
                new_config: Some(configured),
                signing_key_id: Some(key_id),
                signing_key_sha256: Some(fingerprint),
            };
            write_cli_plugin_transaction(manager.root(), &transaction).unwrap();
            std::process::exit(86);
        }
        let execution = into_markdown::ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );
        let (_, project_relative) = project_plugin_store_scope(&project).unwrap();
        let manager = PluginManager::open_existing_scoped(
            &anchor,
            &project_relative,
            global.trusted_signers(),
        )
        .unwrap()
        .unwrap();
        recover_pending_cli_plugin_transaction_at(
            manager.root(),
            &anchor,
            &relative,
            &mut global,
            &execution,
        )
        .unwrap();
        assert_eq!(
            manager.verify("transaction-fixture", &execution).unwrap_err().code,
            ManagerErrorCode::NotInstalled
        );
        assert!(
            config::plugins_in_scope(crate::args::Scope::Project, &project).unwrap().is_empty()
        );
    }

    #[test]
    fn joint_transaction_recovers_cas_before_phase_write_in_new_process() {
        use std::process::Command as ProcessCommand;

        let temporary = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let invoke = |mode: &str| {
            ProcessCommand::new(&executable)
                .args([
                    "--exact",
                    "app::tests::joint_transaction_config_cas_crash_child",
                    "--nocapture",
                ])
                .env("INTO_MD_JOINT_TXN_ROOT", temporary.path())
                .env("INTO_MD_JOINT_TXN_MODE", mode)
                .env("APPDATA", temporary.path().join("appdata"))
                .env("LOCALAPPDATA", temporary.path().join("appdata"))
                .env("XDG_CONFIG_HOME", temporary.path().join("appdata"))
                .env("HOME", temporary.path().join("home"))
                .status()
                .unwrap()
        };
        assert_eq!(invoke("seed").code(), Some(86));
        assert!(invoke("recover").success());
    }

    #[test]
    fn plugin_joint_phase_crash_child() {
        let Some(root) = std::env::var_os("INTO_MD_PLUGIN_PHASE_ROOT") else { return };
        let mode = std::env::var("INTO_MD_PLUGIN_PHASE_MODE").unwrap();
        let root = PathBuf::from(root);
        let project = root.join("project");
        let user_data = root.join("user-data");
        fs::create_dir_all(&project).unwrap();
        create_private_user_data_directory(&user_data).unwrap();
        let _test_config =
            config::TestGlobalConfigGuard::set(Some(user_data.join("config/config.toml")));
        let config_path = config::global_config_path().unwrap();
        ensure_private_user_data_path(config_path.parent().unwrap()).unwrap();
        let _test_user_data = TestUserDataGuard::set(Some(user_data));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if mode == "crash" || mode == "remove-crash" || mode == "indeterminate-seed" {
            let (package, key_id, fingerprint) = signed_transaction_fixture();
            let package_path = root.join("plugin.zip");
            fs::write(&package_path, &package).unwrap();
            let sha256 = format!("{:x}", Sha256::digest(&package));
            run(
                vec![
                    OsString::from("plugins"),
                    OsString::from("install"),
                    package_path.into_os_string(),
                    OsString::from("--sha256"),
                    OsString::from(sha256),
                    OsString::from("--signing-key-id"),
                    OsString::from(key_id),
                    OsString::from("--signing-key-sha256"),
                    OsString::from(fingerprint),
                    OsString::from("--scope"),
                    OsString::from("global"),
                ],
                RunContext {
                    user_data_anchor: Some(root.join("user-data")),
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                    stdin_is_terminal: true,
                    cwd: project,
                },
            )
            .unwrap();
            if mode == "indeterminate-seed" {
                let (anchor, relative) = global_plugin_store_scope().unwrap();
                let store = anchor.join(relative);
                assert!(store.join(PLUGIN_CLI_TRANSACTION).exists());
                assert!(store.join(".trusted-signers.next").exists());
                std::process::exit(87);
            }
            if mode == "remove-crash" {
                stdout.clear();
                stderr.clear();
                run(
                    vec![
                        OsString::from("plugins"),
                        OsString::from("remove"),
                        OsString::from("transaction-fixture"),
                        OsString::from("--scope"),
                        OsString::from("global"),
                    ],
                    RunContext {
                        user_data_anchor: Some(root.join("user-data")),
                        stdout: &mut stdout,
                        stderr: &mut stderr,
                        stdin_is_terminal: true,
                        cwd: root.join("project"),
                    },
                )
                .unwrap();
            }
            panic!("crash hook did not terminate the child");
        }
        run(
            vec![OsString::from("plugins"), OsString::from("--json")],
            RunContext {
                user_data_anchor: Some(root.join("user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: project,
            },
        )
        .unwrap();
        let point = std::env::var("INTO_MD_PLUGIN_PHASE_POINT").unwrap();
        let expected_installed = if point.starts_with("remove-") {
            !point.contains("config-changed")
        } else {
            point.contains("config-changed") || point.contains("trust-published")
        };
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.contains("transaction-fixture"), expected_installed, "{output}");

        let (package, key_id, fingerprint) = signed_transaction_fixture();
        let package_sha256 = format!("{:x}", Sha256::digest(&package));
        let (anchor, relative) = global_plugin_store_scope().unwrap();
        let manager = PluginManager::open_persisted_scoped(&anchor, &relative).unwrap();
        let execution = into_markdown::ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );
        let verified = manager.verify("transaction-fixture", &execution);
        assert_eq!(verified.is_ok(), expected_installed, "{point}");
        match verified {
            Ok(installed) => {
                assert_eq!(installed.package_sha256, package_sha256);
                assert_eq!(installed.signing_key_id, key_id);
            }
            Err(error) => assert_eq!(error.code, ManagerErrorCode::NotInstalled),
        }
        let config_path = config::global_config_path().unwrap();
        let config_lock = config::lock_exact(&config_path).unwrap();
        let configured = config::plugins_in_exact_locked(&config_lock, &config_path).unwrap();
        assert_eq!(configured.contains_key("transaction-fixture"), expected_installed, "{point}");
        if let Some(configured) = configured.get("transaction-fixture") {
            assert!(configured.enabled);
            assert_eq!(configured.sha256.as_deref(), Some(package_sha256.as_str()));
            assert_eq!(configured.signing_key_id, key_id);
            assert_eq!(configured.signing_key_sha256, fingerprint);
        }
        let trust_expected = point.starts_with("remove-") || expected_installed;
        assert_eq!(
            manager.trusted_signers().fingerprints.get(&key_id),
            trust_expected.then_some(&fingerprint),
            "{point}"
        );
        assert!(!manager.root().join(PLUGIN_CLI_TRANSACTION).exists(), "{point}");
        assert!(fs::read_dir(manager.root()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name != ".transaction.json"
                && !name.starts_with(".cli-backup-")
                && !name.starts_with(".backup-")
                && !name.starts_with(".snapshot-next-")
                && !name.starts_with(".incoming-")
                && !name.starts_with(".staging-")
                && !name.starts_with(".removed-")
        }));
        assert!(fs::read_dir(config_path.parent().unwrap()).unwrap().all(|entry| {
            !entry.unwrap().file_name().to_string_lossy().starts_with(".into-md-config-")
        }));
    }

    #[test]
    fn plugin_joint_install_recovers_every_durable_phase_in_new_processes() {
        use std::process::Command as ProcessCommand;

        for phase in [
            "install-started",
            "install-store-changed",
            "install-config-cas",
            "install-config-changed",
            "install-trust-published",
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let executable = std::env::current_exe().unwrap();
            let invoke = |mode: &str| {
                ProcessCommand::new(&executable)
                    .args(["--exact", "app::tests::plugin_joint_phase_crash_child", "--nocapture"])
                    .env("INTO_MD_PLUGIN_PHASE_ROOT", temporary.path())
                    .env("INTO_MD_PLUGIN_PHASE_MODE", mode)
                    .env("INTO_MD_PLUGIN_PHASE_POINT", phase)
                    .env("INTO_MD_PLUGIN_CLI_CRASH_POINT", phase)
                    .env("APPDATA", temporary.path().join("appdata"))
                    .env("LOCALAPPDATA", temporary.path().join("appdata"))
                    .env("XDG_CONFIG_HOME", temporary.path().join("config"))
                    .env("HOME", temporary.path().join("home"))
                    .status()
                    .unwrap()
            };
            assert_eq!(invoke("crash").code(), Some(86), "{phase}");
            assert!(invoke("recover").success(), "{phase}");
        }
    }

    #[test]
    fn plugin_joint_remove_recovers_every_durable_phase_in_new_processes() {
        use std::process::Command as ProcessCommand;

        for phase in
            ["remove-started", "remove-store-changed", "remove-config-cas", "remove-config-changed"]
        {
            let temporary = tempfile::tempdir().unwrap();
            let executable = std::env::current_exe().unwrap();
            let invoke = |mode: &str| {
                ProcessCommand::new(&executable)
                    .args(["--exact", "app::tests::plugin_joint_phase_crash_child", "--nocapture"])
                    .env("INTO_MD_PLUGIN_PHASE_ROOT", temporary.path())
                    .env("INTO_MD_PLUGIN_PHASE_MODE", mode)
                    .env("INTO_MD_PLUGIN_PHASE_POINT", phase)
                    .env("INTO_MD_PLUGIN_CLI_CRASH_POINT", phase)
                    .env("APPDATA", temporary.path().join("appdata"))
                    .env("LOCALAPPDATA", temporary.path().join("appdata"))
                    .env("XDG_CONFIG_HOME", temporary.path().join("config"))
                    .env("HOME", temporary.path().join("home"))
                    .status()
                    .unwrap()
            };
            assert_eq!(invoke("remove-crash").code(), Some(86), "{phase}");
            assert!(invoke("recover").success(), "{phase}");
        }
    }

    #[cfg(feature = "plugin-manager-fault-injection")]
    #[test]
    fn plugin_joint_trust_indeterminate_recovers_forward_in_new_process() {
        use std::process::Command as ProcessCommand;

        let temporary = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let invoke = |mode: &str, inject: bool| {
            let mut command = ProcessCommand::new(&executable);
            command
                .args(["--exact", "app::tests::plugin_joint_phase_crash_child", "--nocapture"])
                .env("INTO_MD_PLUGIN_PHASE_ROOT", temporary.path())
                .env("INTO_MD_PLUGIN_PHASE_MODE", mode)
                .env("INTO_MD_PLUGIN_PHASE_POINT", "install-config-changed")
                .env("APPDATA", temporary.path().join("appdata"))
                .env("LOCALAPPDATA", temporary.path().join("appdata"))
                .env("XDG_CONFIG_HOME", temporary.path().join("config"))
                .env("HOME", temporary.path().join("home"));
            if inject {
                command.env("INTO_MD_PLUGIN_TRUST_INDETERMINATE", "1");
            }
            command.status().unwrap()
        };
        assert_eq!(invoke("indeterminate-seed", true).code(), Some(87));
        assert!(invoke("recover", false).success());
    }

    #[cfg(windows)]
    #[test]
    fn plugin_read_recovery_child() {
        let Some(cwd) = std::env::var_os("INTO_MD_PLUGIN_READ_RECOVERY_ROOT") else { return };
        let cwd = PathBuf::from(cwd);
        let user_data = cwd.parent().unwrap().join("user-data");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![OsString::from("plugins"), OsString::from("--json")],
            RunContext {
                user_data_anchor: Some(user_data),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd,
            },
        )
        .unwrap();
        assert!(String::from_utf8(stdout).unwrap().contains("demo"));
    }

    #[cfg(windows)]
    #[test]
    fn recreated_project_does_not_consume_old_scope_journal_child() {
        let Some(root) = std::env::var_os("INTO_MD_PROJECT_RECREATE_ROOT") else { return };
        let root = PathBuf::from(root);
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        let user_data = root.join("isolated-user-data");
        let _test_user_data = TestUserDataGuard::set(Some(user_data.clone()));
        let (anchor, old_relative) = project_plugin_store_scope(&project).unwrap();
        let global = PluginManager::open_persisted_scoped(
            &global_plugin_store_scope().unwrap().0,
            &global_plugin_store_scope().unwrap().1,
        )
        .unwrap();
        let old =
            PluginManager::open_scoped(&anchor, &old_relative, global.trusted_signers()).unwrap();
        fs::write(old.root().join(PLUGIN_CLI_TRANSACTION), b"old identity sentinel").unwrap();
        fs::rename(&project, root.join("moved")).unwrap();
        fs::create_dir(&project).unwrap();
        let (_, new_relative) = project_plugin_store_scope(&project).unwrap();
        assert_ne!(old_relative, new_relative);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![OsString::from("plugins"), OsString::from("--json")],
            RunContext {
                user_data_anchor: Some(user_data),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: project.clone(),
            },
        )
        .unwrap();
        assert_eq!(String::from_utf8(stdout).unwrap().trim(), "[]");
        assert_eq!(fs::read_dir(&project).unwrap().count(), 0);
        assert_eq!(
            fs::read(old.root().join(PLUGIN_CLI_TRANSACTION)).unwrap(),
            b"old identity sentinel"
        );
    }

    #[cfg(windows)]
    #[test]
    fn recreated_project_scope_isolated_parent() {
        use std::process::Command as ProcessCommand;

        let temporary = tempfile::tempdir().unwrap();
        let appdata = temporary.path().join("appdata");
        fs::create_dir(&appdata).unwrap();
        let status = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::recreated_project_does_not_consume_old_scope_journal_child",
                "--nocapture",
            ])
            .env("INTO_MD_PROJECT_RECREATE_ROOT", temporary.path())
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &appdata)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(windows)]
    #[test]
    fn first_plugin_read_recovers_project_config_before_load() {
        use std::process::Command as ProcessCommand;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let appdata = temporary.path().join("appdata");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&appdata).unwrap();
        let target = project.join(".into-markdown.toml");
        fs::write(&target, "schema_version = 1\n").unwrap();
        let replacement = format!(
            "schema_version = 1\n[plugins.demo]\nsource = \"file:///demo.zip\"\nsha256 = \"{}\"\nprotocol = \"process-v1\"\nenabled = false\nsigning_key_id = \"test\"\nsigning_key_sha256 = \"{}\"\n",
            "0".repeat(64),
            "1".repeat(64)
        );
        let executable = std::env::current_exe().unwrap();
        let crash = ProcessCommand::new(&executable)
            .args([
                "--exact",
                "transaction::windows_config_tests::config_replace_crash_child",
                "--nocapture",
            ])
            .env("INTO_MD_CONFIG_CRASH_ROOT", &project)
            .env("INTO_MD_CONFIG_CRASH_TARGET", ".into-markdown.toml")
            .env("INTO_MD_CONFIG_CRASH_CONTENT", &replacement)
            .env("INTO_MD_CONFIG_CRASH_POINT", "backup")
            .status()
            .unwrap();
        assert_eq!(crash.code(), Some(86));
        assert!(!target.exists());

        let read = ProcessCommand::new(executable)
            .args(["--exact", "app::tests::plugin_read_recovery_child", "--nocapture"])
            .env("INTO_MD_PLUGIN_READ_RECOVERY_ROOT", &project)
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &appdata)
            .status()
            .unwrap();
        assert!(read.success());
        assert_eq!(fs::read_to_string(target).unwrap(), replacement);
    }

    fn invoke(arguments: &[&str], stdin_is_terminal: bool) -> Result<(String, String), CliError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let argument_digest = format!("{:x}", Sha256::digest(arguments.join("\0").as_bytes()));
        let root = std::env::temp_dir()
            .join(format!("into-md-app-test-{}-{argument_digest}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let result = run(
            arguments.iter().map(OsString::from).collect(),
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal,
                cwd: root.clone(),
            },
        );
        fs::remove_dir_all(root).unwrap();
        result?;
        Ok((String::from_utf8(stdout).unwrap(), String::from_utf8(stderr).unwrap()))
    }

    #[cfg(unix)]
    #[test]
    fn one_cli_invocation_recovers_crash_residue_and_reports_success() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let input = root.join("input.txt");
        let output = root.join("output.md");
        let report = root.join("report.json");
        fs::write(&input, b"hello\n").unwrap();

        let output_context = into_markdown::ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );
        let residue = [Target { path: output.clone(), bytes: b"interrupted" }];
        let mut transaction =
            crate::transaction::prepare(&residue, false, &output_context).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![
                input.into_os_string(),
                OsString::from("-o"),
                output.clone().into_os_string(),
                OsString::from("--report"),
                report.clone().into_os_string(),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&output).unwrap(), "hello\n");
        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["succeeded"], 1);
        assert_eq!(report["failed"], 0);
        assert_eq!(report["items"][0]["status"], "success");
    }

    #[test]
    fn terminal_without_input_prints_help() {
        let (stdout, _) = invoke(&[], true).unwrap();
        assert!(stdout.contains("Usage:"));
        assert!(stdout.contains("providers"));
    }

    #[test]
    fn no_config_rejects_plugin_operations_that_require_scope_authority() {
        let error = invoke(&["--no-config", "plugins", "verify", "demo"], true).unwrap_err();
        assert_eq!(error.code(), "usage");
        assert!(error.to_string().contains("only plugin list and show"));
    }

    #[test]
    fn bundle_assets_always_use_internal_prefix_without_external_writes() {
        let output = Path::new("out/document.mdpkg.zip");
        let cwd = Path::new("/work");
        let plans = [
            plan_file_asset_output(EmitKind::Bundle, AssetModeArg::Extract, None, output, cwd),
            plan_file_asset_output(
                EmitKind::Bundle,
                AssetModeArg::Extract,
                Some(Path::new("custom assets")),
                output,
                cwd,
            ),
            plan_stdout_asset_output(
                EmitKind::Bundle,
                AssetModeArg::Extract,
                Some(Path::new("custom assets")),
                Some(Path::new("input.pdf")),
                cwd,
            ),
        ];
        for asset_output in plans {
            let asset_output = asset_output.unwrap();
            assert_eq!(
                asset_output,
                AssetOutputPlan { uri_prefix: Some("assets".into()), external_directory: None }
            );
            assert_bundle_image_href_hits_entry(asset_output.uri_prefix.as_deref().unwrap());
        }
    }

    #[test]
    fn stdout_default_assets_are_relative_to_the_markdown_working_directory() {
        let working_directory = Path::new("/work/output");
        assert_eq!(
            default_stdout_asset_directory(
                Path::new("/different-volume/report.pptx"),
                working_directory
            ),
            working_directory.join("report_assets")
        );
    }

    #[test]
    fn filesystem_asset_paths_are_segment_encoded_and_survive_commonmark() {
        assert_eq!(
            asset_uri_prefix_for_file_with_flavor(
                b"out/document.md",
                "out/assets #?%/中文".as_bytes(),
                b"/work/project",
                PathFlavor::Posix,
            )
            .unwrap(),
            "assets%20%23%3F%25/中文"
        );
        assert_eq!(
            asset_uri_prefix_for_file_with_flavor(
                b"out/nested/document.md",
                "out/nested/../../safe assets/图".as_bytes(),
                b"/work/project",
                PathFlavor::Posix,
            )
            .unwrap(),
            "../../safe%20assets/图"
        );
        let windows_prefix = asset_uri_prefix_for_file_with_flavor(
            br"C:\Users\Docs\document.md",
            r"c:\users\Assets #?%\中文".as_bytes(),
            br"C:\work",
            PathFlavor::Windows,
        )
        .unwrap();
        assert_eq!(windows_prefix, "../Assets%20%23%3F%25/中文");
        let verbatim_windows_prefix = asset_uri_prefix_for_file_with_flavor(
            br"\\?\C:\Users\Docs\document.md",
            br"C:\Users\Assets",
            br"\\?\C:\work",
            PathFlavor::Windows,
        )
        .unwrap();
        assert_eq!(verbatim_windows_prefix, "../Assets");
        let unc_prefix = asset_uri_prefix_for_file_with_flavor(
            br"\\Server\Share\docs\document.md",
            r"\\server\share\assets #?%\中文".as_bytes(),
            br"\\server\share\work",
            PathFlavor::Windows,
        )
        .unwrap();
        assert_eq!(unc_prefix, "../assets%20%23%3F%25/中文");
        let verbatim_unc_prefix = asset_uri_prefix_for_file_with_flavor(
            br"\\?\UNC\Server\Share\docs\document.md",
            br"\\server\share\assets",
            br"\\?\UNC\server\share\work",
            PathFlavor::Windows,
        )
        .unwrap();
        assert_eq!(verbatim_unc_prefix, "../assets");
        let stdout_prefix = asset_uri_prefix_for_stdout_with_flavor(
            "/var/assets #?%/中文".as_bytes(),
            b"/work/project",
            PathFlavor::Posix,
        )
        .unwrap();
        assert_eq!(stdout_prefix, "../../var/assets%20%23%3F%25/中文");
        assert_eq!(
            asset_uri_prefix_for_stdout_with_flavor(
                "assets #?%/中文\\literal".as_bytes(),
                b"/work/project",
                PathFlavor::Posix,
            )
            .unwrap(),
            "assets%20%23%3F%25/中文%5Cliteral"
        );

        for (output, directory, cwd, flavor) in [
            (
                br"C:\docs\document.md".as_slice(),
                br"D:\assets".as_slice(),
                br"C:\work".as_slice(),
                PathFlavor::Windows,
            ),
            (
                br"\\server\share\document.md".as_slice(),
                br"\\server\other\assets".as_slice(),
                br"\\server\share\work".as_slice(),
                PathFlavor::Windows,
            ),
        ] {
            let error =
                asset_uri_prefix_for_file_with_flavor(output, directory, cwd, flavor).unwrap_err();
            assert_eq!(error.code(), "assetPathUnsupported");
            assert_eq!(error.exit_code(), 2);
        }
        assert_eq!(
            asset_uri_prefix_for_file_with_flavor(
                br"/work/document.md",
                br"C:\assets",
                br"/work",
                PathFlavor::Posix,
            )
            .unwrap(),
            "C%3A%5Cassets"
        );
        let error = asset_uri_prefix_for_stdout_with_flavor(
            br"\\server\share\assets",
            br"C:\work",
            PathFlavor::Windows,
        )
        .unwrap_err();
        assert_eq!(error.code(), "assetPathUnsupported");
        for device in [br"\\.\PhysicalDrive0".as_slice(), br"\\?\GLOBALROOT\Device\HarddiskVolume1"]
        {
            let error =
                asset_uri_prefix_for_stdout_with_flavor(device, br"C:\work", PathFlavor::Windows)
                    .unwrap_err();
            assert_eq!(error.code(), "assetPathUnsupported");
        }
    }

    #[test]
    fn rendered_asset_uris_resolve_with_commonmark_and_file_url_semantics() {
        let windows_prefix = asset_uri_prefix_for_file_with_flavor(
            br"C:\Users\Docs\document.md",
            r"c:\users\Assets #?%\中文".as_bytes(),
            br"C:\work",
            PathFlavor::Windows,
        )
        .unwrap();
        let unc_prefix = asset_uri_prefix_for_file_with_flavor(
            br"\\Server\Share\docs\document.md",
            r"\\server\share\assets #?%\中文".as_bytes(),
            br"\\server\share\work",
            PathFlavor::Windows,
        )
        .unwrap();
        let stdout_prefix = asset_uri_prefix_for_stdout_with_flavor(
            "/var/assets #?%/中文".as_bytes(),
            b"/work/project",
            PathFlavor::Posix,
        )
        .unwrap();

        let asset = Asset {
            id: AssetId("uri-image".into()),
            filename: Some("image.png".into()),
            media_type: "image/png".into(),
            bytes: vec![1],
            external_uri: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: asset.id.clone(), alt: Some("image".into()) },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        assert_rendered_asset_resolves(
            &document,
            &asset,
            &windows_prefix,
            "file:///C:/Users/Docs/",
            "file:///C:/Users/Assets%20%23%3F%25/%E4%B8%AD%E6%96%87",
        );
        assert_rendered_asset_resolves(
            &document,
            &asset,
            &unc_prefix,
            "file://server/share/docs/",
            "file://server/share/assets%20%23%3F%25/%E4%B8%AD%E6%96%87",
        );
        assert_rendered_asset_resolves(
            &document,
            &asset,
            &stdout_prefix,
            "file:///work/project/",
            "file:///var/assets%20%23%3F%25/%E4%B8%AD%E6%96%87",
        );
        let mut options = ConversionOptions::default();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("out/document.md");
        let directory = root.path().join("out/nested/../assets #%/中文/literal");
        options.output.asset_uri_prefix =
            Some(asset_uri_prefix_for_file(&output, &directory, root.path()).unwrap());
        let markdown = render_markdown(&document, std::slice::from_ref(&asset), &options).unwrap();
        let href = markdown_image_href(&markdown);
        let result = into_markdown::ConversionResult::new(
            document.clone(),
            markdown,
            vec![asset.clone()],
            vec![],
            vec![],
        );
        output::write_assets(&result, &directory, AssetModeArg::Extract, ConflictPolicy::Error)
            .unwrap();
        let base_url = url::Url::from_directory_path(output.parent().unwrap()).unwrap();
        let resolved = base_url.join(&href).unwrap();
        assert_eq!(resolved.scheme(), "file");
        assert!(resolved.query().is_none());
        assert!(resolved.fragment().is_none());
        assert_eq!(fs::read(resolved.to_file_path().unwrap()).unwrap(), asset.bytes);
    }

    fn assert_rendered_asset_resolves(
        document: &Document,
        asset: &Asset,
        prefix: &str,
        base: &str,
        expected_directory: &str,
    ) {
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some(prefix.into());
        let markdown = render_markdown(document, std::slice::from_ref(asset), &options).unwrap();
        let href = markdown_image_href(&markdown);
        let resolved = url::Url::parse(base).unwrap().join(&href).unwrap();
        let filename = into_markdown::plan_assets(document, std::slice::from_ref(asset), &options)
            .unwrap()
            .entries()[0]
            .filename
            .clone();
        assert_eq!(resolved.as_str(), format!("{expected_directory}/{filename}"));
        assert_eq!(resolved.scheme(), "file");
        assert!(resolved.query().is_none());
        assert!(resolved.fragment().is_none());
        assert!(!href.contains(':'));
        assert!(!href.starts_with("//"));
    }

    fn assert_bundle_image_href_hits_entry(prefix: &str) {
        let asset = Asset {
            id: AssetId("bundle-route-image".into()),
            filename: Some("route.png".into()),
            media_type: "image/png".into(),
            bytes: vec![7, 8, 9],
            external_uri: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: asset.id.clone(), alt: Some("bundle".into()) },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some(prefix.into());
        let markdown = render_markdown(&document, std::slice::from_ref(&asset), &options).unwrap();
        let result =
            into_markdown::ConversionResult::new(document, markdown, vec![asset], vec![], vec![]);
        let bundle = output::encode_result(&result, EmitKind::Bundle).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bundle)).unwrap();
        let markdown = {
            let mut entry = archive.by_name("document.md").unwrap();
            let mut markdown = String::new();
            entry.read_to_string(&mut markdown).unwrap();
            markdown
        };
        let href = markdown_image_href(&markdown);
        assert!(archive.by_name(&href).is_ok(), "missing ZIP entry for image href {href}");
    }

    fn markdown_image_href(markdown: &str) -> String {
        MarkdownParser::new(markdown)
            .find_map(|event| match event {
                Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn english_and_chinese_help_are_available() {
        let (english, _) = invoke(&["--help"], true).unwrap();
        let (chinese, _) = invoke(&["--language", "zh-CN", "--help"], true).unwrap();
        assert!(english.contains("Convert local files"));
        assert!(chinese.contains("将文档转换"));
    }

    #[test]
    fn management_commands_have_stable_json() {
        let (formats, _) = invoke(&["formats", "--json"], true).unwrap();
        let (version, _) = invoke(&["version", "--json"], true).unwrap();
        assert!(formats.contains("\"format\": \"pdf\""));
        assert!(formats.contains("\"source\": \"core\""));
        assert!(formats.contains("\"runtimeComponent\": \"pdfium\""));
        assert!(!formats.contains("\"status\": \"planned\""));
        assert!(!formats.contains("\"format\": \"wikipedia\""));
        assert!(!formats.contains("\"format\": \"youtube\""));
        assert!(version.contains("\"name\": \"into-md\""));
    }

    #[test]
    fn doctor_runtime_checks_come_from_the_core_catalog() {
        let (doctor, _) = invoke(&["doctor", "--deep", "--json"], true).unwrap();
        let checks: serde_json::Value = serde_json::from_str(&doctor).unwrap();
        let checks = checks.as_array().unwrap();
        for (id, hint) in [
            ("runtime.pdfium", "repair the installed into-md Core package"),
            ("runtime.ocr", "setup ocr"),
            ("runtime.asr", "setup media"),
            ("runtime.diarization", "setup media"),
        ] {
            let check = checks.iter().find(|check| check["id"] == id).unwrap();
            assert!(matches!(check["status"].as_str(), Some("ok" | "missing")));
            assert!(check["detail"].as_str().unwrap().contains(hint));
        }
        let ocr = checks.iter().find(|check| check["id"] == "runtime.ocr").unwrap();
        assert_eq!(ocr["status"], "missing");
    }

    #[test]
    fn formats_detect_text_contract_prefers_magic_over_extension() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("misleading.docx");
        fs::write(&input, b"%PDF-1.7\n").unwrap();
        let path = input.to_str().unwrap();
        let (output, _) = invoke(&["formats", "detect", path], true).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "FORMAT\tCONFIDENCE\tEXPLICIT\tDETECTOR\tREASON\tDIAGNOSTICS");
        assert_eq!(lines[1], "pdf\t0.990\tfalse\tbuiltin.detector.content\tPDF magic bytes\t");
        assert_eq!(lines[2], "docx\t0.910\tfalse\tbuiltin.detector.hints\tfilename extension\t");
    }

    #[test]
    fn formats_detect_json_contract_exposes_detector_and_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("misleading.docx");
        fs::write(&input, b"%PDF-1.7\n").unwrap();
        let path = input.to_str().unwrap();
        let (output, _) =
            invoke(&["formats", "detect", path, "--mime-type", "application/json", "--json"], true)
                .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        let candidates = value["candidates"].as_array().unwrap();
        assert_eq!(candidates[0]["format"], "pdf");
        assert_eq!(candidates[0]["detectorId"], "builtin.detector.content");
        assert_eq!(candidates[0]["reason"], "PDF magic bytes");
        assert!(candidates[0]["diagnostics"].as_array().is_some());
        assert_eq!(candidates[1]["format"], "json");
        assert_eq!(candidates[2]["format"], "docx");
        assert_eq!(candidates[1]["diagnostics"][0], "filename extension and media type disagree");
    }

    #[test]
    fn direct_input_uses_no_convert_subcommand() {
        let root = std::env::temp_dir().join(format!("into-md-direct-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let input = root.join("example.pdf");
        fs::write(&input, b"%PDF-scaffold").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![input.into_os_string()],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 9);
        assert_eq!(error.code(), "componentUnavailable");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dry_run_expands_directories_without_writing() {
        let root = std::env::temp_dir().join(format!("into-md-dry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("input/sub")).unwrap();
        fs::write(root.join("input/sub/a.pdf"), b"pdf").unwrap();
        let output = root.join("out");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![
                root.join("input").into_os_string(),
                OsString::from("--recursive"),
                OsString::from("--output-dir"),
                output.clone().into_os_string(),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        let expected = Path::new("sub").join("a.md");
        assert!(String::from_utf8(stdout).unwrap().contains(expected.to_string_lossy().as_ref()));
        assert!(!output.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn single_file_creates_missing_output_root_and_report() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let input = root.join("document.txt");
        let output = root.join("missing-output");
        let report = root.join("report.json");
        fs::write(&input, b"transactional output\n").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let result = run(
            vec![
                OsString::from("--no-config"),
                input.into_os_string(),
                OsString::from("--output-dir"),
                output.clone().into_os_string(),
                OsString::from("--conflict"),
                OsString::from("error"),
                OsString::from("--report"),
                report.clone().into_os_string(),
                OsString::from("--ocr"),
                OsString::from("off"),
                OsString::from("--asset-mode"),
                OsString::from("omit"),
                OsString::from("--progress"),
                OsString::from("never"),
            ],
            RunContext {
                user_data_anchor: Some(root.join("user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        );

        assert!(
            result.is_ok(),
            "conversion failed: {result:?}; stdout={}; stderr={}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(
            fs::read_to_string(output.join("document.md")).unwrap(),
            "transactional output\n"
        );
        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["succeeded"], 1);
        assert_eq!(report["failed"], 0);
    }

    #[test]
    fn recursive_batch_creates_missing_output_tree_and_report() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let input = root.join("input");
        let output = root.join("missing-output");
        let report = root.join("reports/batch.json");
        fs::create_dir_all(input.join("nested")).unwrap();
        fs::write(input.join("first.txt"), b"first\n").unwrap();
        fs::write(input.join("nested/second.txt"), b"second\n").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let result = run(
            vec![
                OsString::from("--no-config"),
                input.into_os_string(),
                OsString::from("--recursive"),
                OsString::from("--output-dir"),
                output.clone().into_os_string(),
                OsString::from("--jobs"),
                OsString::from("4"),
                OsString::from("--report"),
                report.clone().into_os_string(),
                OsString::from("--ocr"),
                OsString::from("off"),
                OsString::from("--asset-mode"),
                OsString::from("omit"),
                OsString::from("--progress"),
                OsString::from("never"),
            ],
            RunContext {
                user_data_anchor: Some(root.join("user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        );

        assert!(
            result.is_ok(),
            "batch failed: {result:?}; stdout={}; stderr={}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(fs::read_to_string(output.join("first.md")).unwrap(), "first\n");
        assert_eq!(fs::read_to_string(output.join("nested/second.md")).unwrap(), "second\n");
        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["succeeded"], 2);
        assert_eq!(report["failed"], 0);
    }

    #[test]
    fn parallel_batch_serializes_atomic_output_leases() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let input = root.join("input");
        let output = root.join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();
        let mut arguments = Vec::new();
        for index in 0..24 {
            let path = input.join(format!("item-{index:02}.txt"));
            fs::write(&path, format!("parallel item {index}\n")).unwrap();
            arguments.push(path.into_os_string());
        }
        arguments.extend([
            OsString::from("--jobs"),
            OsString::from("8"),
            OsString::from("--output-dir"),
            output.clone().into_os_string(),
            OsString::from("--progress"),
            OsString::from("never"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run(
            arguments,
            RunContext {
                user_data_anchor: Some(root.join("user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        );
        assert!(
            result.is_ok(),
            "batch failed: {result:?}; stdout={}; stderr={}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        for index in 0..24 {
            let path = output.join(format!("item-{index:02}.md"));
            assert!(path.is_file(), "missing {}", path.display());
        }
        assert!(!String::from_utf8(stderr).unwrap().contains("transactionBusy"));
    }

    #[test]
    fn colliding_input_roots_receive_stable_root_prefixes() {
        let output = PathBuf::from("out");
        let item = |root_label: &str, display: &str| WorkItem {
            input: InputRef::Path(PathBuf::from(display)),
            display: display.into(),
            relative: PathBuf::from("same.pdf"),
            root_label: root_label.into(),
            input_root: PathBuf::from(root_label),
            from_directory: false,
            local_path: Some(PathBuf::from(display)),
        };
        let mut plans = plan_outputs(
            vec![item("left", "left/same.pdf"), item("right", "right/same.pdf")],
            None,
            Some(&output),
            EmitKind::Markdown,
        );
        disambiguate_planned_outputs(&mut plans);
        assert_eq!(plans[0].output.as_deref(), Some(Path::new("out/left/same.md")));
        assert_eq!(plans[1].output.as_deref(), Some(Path::new("out/right/same.md")));
    }

    #[test]
    fn same_root_stem_collisions_keep_the_relative_directory() {
        let output = PathBuf::from("out");
        let item = |name: &str| WorkItem {
            input: InputRef::Path(PathBuf::from("public-web").join(name)),
            display: name.into(),
            relative: PathBuf::from(name),
            root_label: "public-web".into(),
            input_root: PathBuf::from("public-web"),
            from_directory: true,
            local_path: Some(PathBuf::from("public-web").join(name)),
        };
        let mut plans = plan_outputs(
            vec![item("atom.xml"), item("atom.rss")],
            None,
            Some(&output),
            EmitKind::Markdown,
        );

        disambiguate_planned_outputs(&mut plans);

        assert_eq!(plans[0].output.as_deref(), Some(Path::new("out/atom.xml.md")));
        assert_eq!(plans[1].output.as_deref(), Some(Path::new("out/atom.rss.md")));
    }

    #[test]
    fn service_assembly_is_scoped_to_reachable_input_capabilities() {
        let plan = |name: &str| WorkPlan {
            item: WorkItem {
                input: InputRef::Path(PathBuf::from(name)),
                display: name.into(),
                relative: PathBuf::from(name),
                root_label: "input".into(),
                input_root: PathBuf::from("input"),
                from_directory: false,
                local_path: Some(PathBuf::from(name)),
            },
            output: None,
            output_root: None,
        };
        let mut options = ConversionOptions::default();
        let office = invocation_capabilities(&[plan("report.xls")], None, None, &options);
        assert!(office.legacy_office);
        assert!(!office.ocr);
        assert!(!office.transcription);
        assert!(!office.diarization);

        options.ocr.policy = OcrPolicy::Always;
        let office_with_explicit_ocr =
            invocation_capabilities(&[plan("slides.ppt")], None, None, &options);
        assert!(office_with_explicit_ocr.legacy_office);
        assert!(office_with_explicit_ocr.ocr);

        options = ConversionOptions::default();
        let image = invocation_capabilities(&[plan("scan.png")], None, None, &options);
        assert!(image.ocr);

        let media = invocation_capabilities(&[plan("meeting.webm")], None, None, &options);
        assert!(media.transcription);
        assert!(media.diarization);
        assert!(!media.legacy_office);
        assert!(!media.ocr);

        let unknown = invocation_capabilities(&[plan("opaque")], None, None, &options);
        assert!(unknown.ocr && unknown.transcription && unknown.diarization);
        assert!(unknown.legacy_office);
    }

    #[test]
    fn directory_input_requires_output_dir_even_with_one_match() {
        let root = std::env::temp_dir().join(format!("into-md-one-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("input")).unwrap();
        fs::write(root.join("input/one.pdf"), b"pdf").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                root.join("input").into_os_string(),
                OsString::from("--recursive"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stdin_cannot_be_combined_with_files() {
        let error = expand_inputs(
            &ConversionArgs {
                inputs: vec![OsString::from("-"), OsString::from("x.pdf")],
                ..ConversionArgs::default()
            },
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn private_network_needs_additional_authorization() {
        let mut options = ConversionOptions::default();
        options.network.enabled = true;
        for uri in [
            "http://127.0.0.1/document.pdf",
            "http://LOCALHOST./document.pdf",
            "http://[::1]/document.pdf",
            "http://[fc00::1]/document.pdf",
            "http://[fe80::1]/document.pdf",
            "http://[::ffff:127.0.0.1]/document.pdf",
            "http://[::ffff:10.0.0.1]/document.pdf",
        ] {
            let error = validate_input_network(&InputRef::Uri(uri.into()), &options).unwrap_err();
            assert_eq!(error.code(), "privateNetworkDenied");
        }
        for uri in [
            "https://8.8.8.8/document.pdf",
            "https://[2001:4860:4860::8888]/document.pdf",
            "https://[::ffff:8.8.8.8]/document.pdf",
        ] {
            validate_input_network(&InputRef::Uri(uri.into()), &options).unwrap();
        }
    }

    #[test]
    fn cli_remote_source_requires_both_authorizations_and_converts_when_present() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let read = stream.read(&mut request).unwrap();
            assert!(
                std::str::from_utf8(&request[..read])
                    .unwrap()
                    .starts_with("GET /text HTTP/1.1\r\n")
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello\n").unwrap();
        });
        let root = std::env::temp_dir().join(format!("into-md-http-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![
                OsString::from("--no-config"),
                OsString::from(format!("http://{address}/text")),
                OsString::from("--allow-network"),
                OsString::from("--allow-private-network"),
                OsString::from("--allow-host"),
                OsString::from("127.0.0.1"),
                OsString::from("--format"),
                OsString::from("text"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(String::from_utf8(stdout).unwrap(), "hello\n");
        let timing = String::from_utf8(stderr).unwrap();
        assert!(timing.contains("timing: total "));
        assert!(timing.contains("timing: batch wall "));
        assert!(!timing.contains(&address.to_string()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signed_uri_secrets_never_enter_stdout_stderr_or_report() {
        const CANARY: &str = "SIGNED_QUERY_CANARY";
        let root =
            std::env::temp_dir().join(format!("into-md-http-redaction-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mut dry_stdout = Vec::new();
        let mut dry_stderr = Vec::new();
        run(
            vec![
                OsString::from(format!(
                    "https://user:{CANARY}@example.test/input.txt?token={CANARY}#{CANARY}"
                )),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut dry_stdout,
                stderr: &mut dry_stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&dry_stdout).contains(CANARY));
        assert!(!String::from_utf8_lossy(&dry_stderr).contains(CANARY));

        let items = expand_inputs(
            &ConversionArgs {
                inputs: vec![OsString::from(format!(
                    "https://example.test/input.txt?token={CANARY}#{CANARY}"
                ))],
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert!(matches!(&items[0].input, InputRef::Uri(uri) if uri.contains(CANARY)));
        assert!(!items[0].display.contains(CANARY));
        let report = BatchItemReport {
            input: items[0].display.clone(),
            output: None,
            format: None,
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: Vec::new(),
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: Vec::new(),
            duration_ms: Some(0.0),
            processing_duration_ms: None,
        };
        let report_json = BatchReport::try_new(vec![report]).unwrap().to_pretty_json().unwrap();
        assert!(!report_json.contains(CANARY));
        let mut stderr = Vec::new();
        write_stderr_event(
            &mut stderr,
            true,
            "error",
            "canary",
            "safe",
            Some(&items[0].display),
            "safe",
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&stderr).contains(CANARY));

        let malformed = format!("https://example.test:bad/input?token={CANARY}");
        let error = expand_inputs(
            &ConversionArgs { inputs: vec![OsString::from(malformed)], ..Default::default() },
            None,
            None,
        )
        .unwrap_err();
        assert!(!error.to_string().contains(CANARY));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_line_host_allowlist_can_only_narrow_configured_hosts() {
        let mut configured_only = vec!["CONFIGURED.example".into()];
        narrow_allowed_hosts(&mut configured_only, &[]).unwrap();
        assert_eq!(configured_only, vec!["configured.example"]);

        let mut requested_only = Vec::new();
        narrow_allowed_hosts(&mut requested_only, &["REQUESTED.example.".into()]).unwrap();
        assert_eq!(requested_only, vec!["requested.example"]);

        let mut allowed = vec!["EXAMPLE.com.".into(), "api.example.com".into()];
        narrow_allowed_hosts(&mut allowed, &["example.com".into(), "unconfigured.example".into()])
            .unwrap();
        assert_eq!(allowed, vec!["example.com"]);

        let error = narrow_allowed_hosts(&mut allowed, &["other.example".into()]).unwrap_err();
        assert_eq!(error.code(), "hostAllowlistConflict");
        assert_eq!(error.exit_code(), 5);
    }

    #[test]
    fn provider_connection_tests_use_persisted_policies_without_cross_contamination() {
        use std::net::TcpListener;

        fn models_server() -> (u16, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let size = stream.read(&mut request).unwrap();
                assert!(request[..size].starts_with(b"GET /v1/models HTTP/1.1\r\n"));
                let body = br#"{"object":"list","data":[{"id":"fixture-model","object":"model","created":1,"owned_by":"fixture"}]}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            });
            (port, handle)
        }

        let (a_port, a_server) = models_server();
        let (b_port, b_server) = models_server();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let secret_environment = controlled_test_secret_environment();
        fs::write(
            &path,
            format!(
                r#"
schema_version = 1

[providers.a]
type = "openai-compatible"
base_url = "http://127.0.0.1:{a_port}/v1"
model = "fixture-model"
api_key_env = "{secret_environment}"
capabilities = ["image-description"]
allowed_hosts = ["127.0.0.1"]
allow_private_network = true

[providers.b]
type = "openai-compatible"
base_url = "http://localhost:{b_port}/v1"
model = "fixture-model"
api_key_env = "{secret_environment}"
capabilities = ["image-description"]
allowed_hosts = ["localhost"]
allow_private_network = true
"#
            ),
        )
        .unwrap();
        let mut loaded = config::load(root.path(), &[path], true, None, None).unwrap();
        let arguments = |name: &str| crate::args::ProviderTestArgs {
            name: name.into(),
            allow_network: true,
            allow_private_network: true,
            allow_host: Vec::new(),
        };
        assert!(test_provider(&loaded, &arguments("a")).unwrap().configured_model_available);
        loaded.effective.providers.get_mut("a").unwrap().allowed_hosts =
            vec!["draft.invalid".into()];
        assert!(test_provider(&loaded, &arguments("b")).unwrap().configured_model_available);
        a_server.join().unwrap();
        b_server.join().unwrap();
    }

    #[test]
    fn disjoint_config_and_cli_host_allowlists_fail_before_networking() {
        let root = std::env::temp_dir().join(format!("into-md-host-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.toml"),
            "schema_version = 1\n[conversion.network]\nallowed_hosts = [\"configured.example\"]\n",
        )
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                OsString::from("--no-config"),
                OsString::from("--config"),
                root.join("config.toml").into_os_string(),
                OsString::from("https://requested.example/document.pdf"),
                OsString::from("--allow-network"),
                OsString::from("--allow-host"),
                OsString::from("requested.example"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "hostAllowlistConflict");
        assert_eq!(error.exit_code(), 5);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_test_requires_private_network_authorization() {
        let root = std::env::temp_dir().join(format!("into-md-provider-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("provider.toml"),
            r#"schema_version = 1
[providers.local]
type = "openai-compatible"
base_url = "http://127.0.0.1:11434/v1"
model = "vision"
api_key_env = "LOCAL_KEY"
"#,
        )
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                OsString::from("--no-config"),
                OsString::from("--config"),
                root.join("provider.toml").into_os_string(),
                OsString::from("providers"),
                OsString::from("test"),
                OsString::from("local"),
                OsString::from("--allow-network"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "privateNetworkDenied");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_test_enforces_configured_host_allowlist_before_backend() {
        let root =
            std::env::temp_dir().join(format!("into-md-provider-allowlist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("provider.toml"),
            r#"schema_version = 1
[conversion.network]
allowed_hosts = ["allowed.example"]
[providers.remote]
type = "openai-compatible"
base_url = "https://other.example/v1"
model = "vision"
api_key_env = "REMOTE_KEY"
capabilities = ["vision-ocr"]
[providers.remote.models]
vision-ocr = "vision"
"#,
        )
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                OsString::from("--no-config"),
                OsString::from("--config"),
                root.join("provider.toml").into_os_string(),
                OsString::from("providers"),
                OsString::from("test"),
                OsString::from("remote"),
                OsString::from("--allow-network"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "hostDenied");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enabled_ai_provider_uses_effective_url_policy_before_conversion() {
        let root = std::env::temp_dir().join(format!("into-md-ai-provider-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("provider.toml"),
            r#"schema_version = 1
default_provider = "remote"
[conversion.network]
allowed_hosts = ["allowed.example"]
[conversion.ai]
vision_ocr = "only"
[capability_routes.ocr]
mode = "only"
primary = "provider:remote/vision-ocr"
[providers.remote]
type = "openai-compatible"
base_url = "https://other.example/v1"
model = "vision"
api_key_env = "REMOTE_KEY"
capabilities = ["vision-ocr"]
[providers.remote.models]
vision-ocr = "vision"
"#,
        )
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                OsString::from("--no-config"),
                OsString::from("--config"),
                root.join("provider.toml").into_os_string(),
                OsString::from("-"),
                OsString::from("--allow-network"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: false,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "hostDenied");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_audio_transcription_does_not_require_ai_provider() {
        let root = std::env::temp_dir().join(format!("into-md-local-asr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let input = root.join("meeting.wav");
        fs::write(&input, b"RIFF").unwrap();
        let config = root.join("provider.toml");
        fs::write(
            &config,
            r#"schema_version = 1
default_provider = "blocked"
[providers.blocked]
type = "openai-compatible"
base_url = "http://127.0.0.1:9/v1"
model = "unused"
api_key_env = "MISSING_LOCAL_ASR_TEST_KEY"
"#,
        )
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![
                OsString::from("--no-config"),
                OsString::from("--config"),
                config.into_os_string(),
                input.into_os_string(),
                OsString::from("--ai"),
                OsString::from("audio-transcription=only"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        assert!(String::from_utf8(stdout).unwrap().contains("meeting.wav"));
        assert!(stderr.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_diarization_does_not_require_ai_provider() {
        let root =
            std::env::temp_dir().join(format!("into-md-local-diarize-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let input = root.join("meeting.wav");
        fs::write(&input, b"RIFF").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            vec![
                OsString::from("--no-config"),
                input.into_os_string(),
                OsString::from("--diarize"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        assert!(String::from_utf8(stdout).unwrap().contains("meeting.wav"));
        assert!(stderr.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_ai_capability_still_requires_provider() {
        let root = std::env::temp_dir().join(format!("into-md-remote-ai-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let input = root.join("document.txt");
        fs::write(&input, b"hello").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run(
            vec![
                OsString::from("--no-config"),
                input.into_os_string(),
                OsString::from("--ai"),
                OsString::from("layout-repair=only"),
                OsString::from("--dry-run"),
            ],
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("requires --ai-provider"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn network_url_policy_normalizes_idn_and_ipv6_provider_targets() {
        let mut options = ConversionOptions::default();
        options.network.enabled = true;
        options.network.allowed_hosts = vec!["xn--bcher-kva.example".into()];
        validate_network_url("https://bücher.example/v1", &options, "provider").unwrap();

        options.network.allowed_hosts = vec!["[2001:4860:4860::8888]".into()];
        validate_network_url("https://[2001:4860:4860:0:0:0:0:8888]/v1", &options, "provider")
            .unwrap();

        for url in ["https://[fc00::1]/v1", "https://[fe80::1]/v1", "https://[::ffff:10.0.0.1]/v1"]
        {
            options.network.allowed_hosts.clear();
            let error = validate_network_url(url, &options, "provider").unwrap_err();
            assert_eq!(error.code(), "privateNetworkDenied");
        }

        for url in ["ftp://example.com/v1", "https://user@example.com/v1"] {
            let error = validate_network_url(url, &options, "provider").unwrap_err();
            assert_eq!(error.code(), "networkUrlDenied");
        }
    }

    #[test]
    fn plugin_url_requires_a_hash_before_backend_dispatch() {
        let error =
            invoke(&["plugins", "install", "https://example.com/plugin.zip"], true).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--sha256"));
    }

    #[test]
    fn provider_configuration_never_accepts_plaintext_key_argument() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        assert!(!help.contains("--api-key "));
    }

    #[test]
    fn capability_candidates_preserve_provider_protocol_ids() {
        let capabilities = vec!["vision-ocr".to_owned(), "audio-transcription".to_owned()];
        assert_eq!(provider_capability_id(&capabilities, "ocr"), Some("vision-ocr"));
        assert_eq!(
            provider_capability_id(&capabilities, "transcription"),
            Some("audio-transcription")
        );
        assert_eq!(provider_capability_id(&capabilities, "diarization"), None);
    }

    #[test]
    fn built_in_ocr_projects_a_core_authority_without_plugin_management() {
        let temporary = tempfile::tempdir().unwrap();
        let loaded = crate::config::load(temporary.path(), &[], true, None, None).unwrap();
        let inspected = std::collections::BTreeMap::from([
            ("official.ocr.ppocrv6", PluginInspection::BuiltIn),
            ("official.media.whisper", PluginInspection::NotInstalled),
        ]);
        let capabilities = capability_views_from_inspections(&loaded, &inspected).unwrap();
        let ocr = capabilities.iter().find(|item| item.id == "ocr").unwrap();
        assert_eq!(ocr.status, "ready");
        assert_eq!(ocr.local_status, "ready");
        assert_eq!(ocr.current_source, CORE_OCR_SOURCE);
        assert_eq!(ocr.current_source_name, "内置 OCR");
        assert_eq!(ocr.sources, [CORE_OCR_SOURCE, "off"]);
        assert!(ocr.sources.iter().all(|source| !source.starts_with("plugin:")));
        assert_eq!(ocr.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(ocr.local_version, ocr.version);
    }

    #[test]
    fn capability_verification_targets_core_only_for_embedded_ocr() {
        assert!(uses_embedded_ocr_verification("ocr", true));
        assert!(!uses_embedded_ocr_verification("ocr", false));
        assert!(!uses_embedded_ocr_verification("transcription", true));
        assert!(!uses_embedded_ocr_verification("missing", true));
    }

    #[test]
    fn embedded_ocr_verification_json_uses_public_core_identity_and_version() {
        let verification = core_ocr_verification_json(17);
        assert_eq!(verification["schemaVersion"], 1);
        assert_eq!(verification["capability"], "ocr");
        assert_eq!(verification["source"], CORE_OCR_SOURCE);
        assert_eq!(verification["sourceName"], "内置 OCR");
        assert_eq!(verification["status"], "ready");
        assert_eq!(verification["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(verification["elapsedMs"], 17);
        assert_eq!(verification["sharedCapabilities"], serde_json::json!(["ocr"]));
        assert!(verification.get("plugin").is_none());
        assert!(verification.get("pluginName").is_none());
    }

    #[test]
    fn sha256_is_available_for_future_local_plugin_records() {
        let digest = Sha256::digest(b"plugin");
        assert_eq!(format!("{digest:x}").len(), 64);
    }

    #[test]
    fn project_plugin_scope_is_identity_bound_and_isolates_projects() {
        let temporary = tempfile::tempdir().unwrap();
        let _test_user_data = TestUserDataGuard::set(Some(temporary.path().join("user-data")));
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let (anchor_one, scope_one) = project_plugin_store_scope(&first).unwrap();
        let (anchor_two, scope_two) = project_plugin_store_scope(&second).unwrap();
        assert_eq!(anchor_one, anchor_two);
        assert_ne!(scope_one, scope_two);
        assert_eq!(project_plugin_store_scope(&first).unwrap().1, scope_one);
        fs::write(first.join(".into-markdown.toml"), "schema_version = 1\n").unwrap();
        let resolved = ProjectScopeAuthority::resolve(&first).unwrap();
        let nested = first.join("nested").join("deeper");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(project_plugin_store_scope(&nested).unwrap().1, scope_one);

        let moved = temporary.path().join("moved");
        fs::rename(&first, &moved).unwrap();
        fs::create_dir(&first).unwrap();
        assert!(resolved.verify().is_err(), "old project identity accepted replacement");
        let replacement_scope = project_plugin_store_scope(&first).unwrap().1;
        let moved_scope = project_plugin_store_scope(&moved).unwrap().1;
        assert_ne!(replacement_scope, scope_one);
        assert_ne!(moved_scope, scope_one);

        let alias = temporary.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_dir(&first, &alias);
        if alias.exists() {
            let (_, alias_scope) = project_plugin_store_scope(&alias).unwrap();
            assert_eq!(replacement_scope, alias_scope);
        }
    }

    #[test]
    fn reserved_command_filename_can_be_passed_after_double_dash() {
        let parsed = Cli::try_parse_from(["into-md", "--", "formats"]).unwrap();
        assert!(parsed.command.is_none());
        assert_eq!(parsed.conversion.inputs, vec![OsString::from("formats")]);
    }
}
