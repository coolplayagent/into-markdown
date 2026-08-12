//! CLI orchestration, input expansion, policy application, and management commands.

use crate::args::{
    AssetModeArg, Cli, Command, CompletionShell, ConfigCommand, ConfigOutputFormat, ConflictPolicy,
    ConversionArgs, DetectArgs, EmitKind, FormatsCommand, LogFormat, ModelsCommand, OcrPolicyArg,
    PluginsCommand, ProfileCommand, ProviderType, ProvidersCommand,
};
use crate::config::{self, LoadedConfig, ProviderConfig};
use crate::error::{CliError, ExitClass};
use crate::i18n::{self, Catalog};
use crate::output::{self, BatchItemReport, BatchItemStatus, BatchReport};
use clap::{CommandFactory, Parser};
use globset::{Glob, GlobSet, GlobSetBuilder};
use into_markdown::{
    AiMode, AssetMode, ConversionOptions, ConversionRequest, DetectionRequest, FormatHint,
    InputFormat, InputRef, OcrPolicy,
};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

/// Process services supplied by the binary or tests.
pub struct RunContext<'a> {
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    pub stdin_is_terminal: bool,
    pub cwd: PathBuf,
}

/// Parse and execute one CLI invocation.
pub fn run(arguments: Vec<OsString>, mut context: RunContext<'_>) -> Result<(), CliError> {
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
    let result = match cli.command {
        None => {
            run_conversion(cli.conversion, &cli.global, loaded, catalog, json_log, &mut context)
        }
        Some(command) => run_command(command, &cli.global, loaded, catalog, json_log, &mut context),
    };
    result.map_err(|error| error.with_rendering(language, json_log))
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
        Command::Models(arguments) => {
            run_models(arguments.command, arguments.json, catalog, context)
        }
        Command::Providers(arguments) => {
            run_providers(arguments.command, arguments.json, &loaded, catalog, context)
        }
        Command::Plugins(arguments) => {
            run_plugins(arguments.command, arguments.json, &loaded, catalog, context)
        }
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FormatView<'a> {
    format: &'a str,
    family: &'a str,
    status: &'a str,
    extensions: &'a [&'a str],
}

fn list_formats(
    family: Option<&str>,
    status: Option<&str>,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let views = into_markdown::planned_formats()
        .iter()
        .filter(|descriptor| family.is_none_or(|family| descriptor.family == family))
        .filter(|descriptor| status.is_none_or(|status| descriptor.status.as_str() == status))
        .map(|descriptor| FormatView {
            format: descriptor.format.as_str(),
            family: descriptor.family,
            status: descriptor.status.as_str(),
            extensions: descriptor.extensions,
        })
        .collect::<Vec<_>>();
    if json {
        write_json(stdout, &views)
    } else {
        writeln!(stdout, "FORMAT\tFAMILY\tSTATUS\tEXTENSIONS")?;
        for view in views {
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}",
                view.format,
                view.family,
                view.status,
                view.extensions.join(",")
            )?;
        }
        Ok(())
    }
}

fn show_format(value: &str, json: bool, stdout: &mut dyn Write) -> Result<(), CliError> {
    let descriptor =
        find_format(value).ok_or_else(|| CliError::usage(format!("unknown format '{value}'")))?;
    let view = FormatView {
        format: descriptor.format.as_str(),
        family: descriptor.family,
        status: descriptor.status.as_str(),
        extensions: descriptor.extensions,
    };
    if json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "format: {}", view.format)?;
        writeln!(stdout, "family: {}", view.family)?;
        writeln!(stdout, "status: {}", view.status)?;
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

fn run_models(
    command: Option<ModelsCommand>,
    json: bool,
    catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    let manifest = into_markdown::model_manifest().map_err(CliError::from)?;
    match command {
        None => {
            if json {
                write_json(context.stdout, &manifest)
            } else {
                writeln!(context.stdout, "MODEL\tDEFAULT\tRUNTIME\tLANGUAGES\tSTATUS")?;
                for bundle in manifest.bundles {
                    writeln!(
                        context.stdout,
                        "{}\t{}\t{}\t{}\tplanned",
                        bundle.id,
                        bundle.id == manifest.default_bundle,
                        bundle.runtime_format,
                        bundle.languages.join(",")
                    )?;
                }
                Ok(())
            }
        }
        Some(ModelsCommand::Show { id, json }) => {
            let bundle = manifest
                .bundles
                .iter()
                .find(|bundle| bundle.id == id)
                .ok_or_else(|| CliError::usage(format!("unknown model bundle '{id}'")))?;
            if json {
                write_json(context.stdout, bundle)
            } else {
                writeln!(context.stdout, "model: {}", bundle.id)?;
                writeln!(context.stdout, "upstream: {}", bundle.upstream_version)?;
                writeln!(context.stdout, "runtime: {}", bundle.runtime_format)?;
                writeln!(context.stdout, "languages: {}", bundle.languages.join(", "))?;
                Ok(())
            }
        }
        Some(ModelsCommand::Install { id }) => Err(CliError::component(format!(
            "models install {}: {}",
            id.as_deref().unwrap_or(&manifest.default_bundle),
            catalog.unavailable()
        ))),
        Some(ModelsCommand::Verify { id, json: _ }) => Err(CliError::component(format!(
            "models verify {}: {}",
            id.as_deref().unwrap_or(&manifest.default_bundle),
            catalog.unavailable()
        ))),
        Some(ModelsCommand::Remove { id }) => {
            Err(CliError::component(format!("models remove {id}: {}", catalog.unavailable())))
        }
        Some(ModelsCommand::Path { id }) => {
            Err(CliError::component(format!("models path {id}: {}", catalog.unavailable())))
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderView<'a> {
    name: &'a str,
    provider_type: &'a str,
    base_url: String,
    model: &'a str,
    api_key_env: &'a str,
    capabilities: &'a [String],
    default: bool,
}

fn run_providers(
    command: Option<ProvidersCommand>,
    json: bool,
    loaded: &LoadedConfig,
    catalog: Catalog,
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
            for capability in &arguments.capability {
                config::validate_capability(capability)?;
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
                api_key_env: arguments.api_key_env,
                timeout_ms: arguments.timeout,
                capabilities: arguments.capability,
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
            if !loaded.effective.providers.contains_key(&name) {
                return Err(CliError::usage(format!("unknown provider '{name}'")));
            }
            let path = config::set_default_provider(scope, &context.cwd, &name)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(ProvidersCommand::Test(arguments)) => {
            let provider =
                loaded.effective.providers.get(&arguments.name).ok_or_else(|| {
                    CliError::usage(format!("unknown provider '{}'", arguments.name))
                })?;
            let mut options = loaded.options.clone();
            apply_network_authorization(
                &mut options,
                arguments.allow_network,
                arguments.allow_private_network,
                &arguments.allow_host,
            )?;
            validate_network_url(&provider.base_url, &options, "provider")?;
            Err(CliError::component(format!(
                "providers test {}: {}",
                arguments.name,
                catalog.unavailable()
            )))
        }
    }
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
        api_key_env: &provider.api_key_env,
        capabilities: &provider.capabilities,
        default: loaded.effective.default_provider.as_deref() == Some(name),
    }
}

fn run_plugins(
    command: Option<PluginsCommand>,
    json: bool,
    loaded: &LoadedConfig,
    catalog: Catalog,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
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
        Some(PluginsCommand::Install { source, sha256, scope: _ }) => {
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
            Err(CliError::component(format!("plugins install: {}", catalog.unavailable())))
        }
        Some(PluginsCommand::Verify { id, json: _ }) => Err(CliError::component(format!(
            "plugins verify {}: {}",
            id.as_deref().unwrap_or("all"),
            catalog.unavailable()
        ))),
        Some(PluginsCommand::Enable { id, scope }) => {
            let path = config::set_plugin_enabled(scope, &context.cwd, &id, true)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Disable { id, scope }) => {
            let path = config::set_plugin_enabled(scope, &context.cwd, &id, false)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
        Some(PluginsCommand::Remove { id, scope }) => {
            let path = config::remove_plugin(scope, &context.cwd, &id)?;
            writeln!(context.stdout, "{}", path.display())?;
            Ok(())
        }
    }
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
                writeln!(context.stdout, "global: {}", loaded.paths.global.display())?;
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
struct DoctorCheck {
    id: String,
    status: String,
    detail: String,
}

fn run_doctor(
    arguments: &crate::args::DoctorArgs,
    loaded: &LoadedConfig,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
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
            id: "modelManifest".into(),
            status: "ok".into(),
            detail: into_markdown::model_manifest().map_or_else(
                |error| error.to_string(),
                |manifest| format!("{} bundle(s)", manifest.bundles.len()),
            ),
        },
        DoctorCheck {
            id: "modelFiles".into(),
            status: "unavailable".into(),
            detail: "model installation and path backend is not implemented".into(),
        },
        DoctorCheck {
            id: "onnxRuntime".into(),
            status: "unavailable".into(),
            detail: "runtime loading is not implemented".into(),
        },
        DoctorCheck {
            id: "temporaryDirectory".into(),
            status: if std::env::temp_dir().is_dir() { "ok" } else { "error" }.into(),
            detail: std::env::temp_dir().display().to_string(),
        },
    ];
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
    for (id, plugin) in &loaded.effective.plugins {
        let (status, detail) = if !plugin.enabled {
            ("disabled", "plugin is disabled".into())
        } else if plugin.source.starts_with("https://") {
            ("unavailable", "installed plugin package lookup is not implemented".into())
        } else if Path::new(&plugin.source).is_file() {
            (
                "unavailable",
                format!("package present; {} execution is not implemented", plugin.protocol),
            )
        } else {
            ("missing", format!("package not found: {}", plugin.source))
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
    if arguments.json {
        write_json(context.stdout, &checks)
    } else {
        writeln!(context.stdout, "CHECK\tSTATUS\tDETAIL")?;
        for check in checks {
            writeln!(context.stdout, "{}\t{}\t{}", check.id, check.status, check.detail)?;
        }
        Ok(())
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
    hint: FormatHint,
    emit: EmitKind,
    asset_mode: AssetModeArg,
    conflict: ConflictPolicy,
    assets_dir: Option<PathBuf>,
}

fn run_conversion(
    arguments: ConversionArgs,
    global: &crate::args::GlobalArgs,
    mut loaded: LoadedConfig,
    catalog: Catalog,
    json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<(), CliError> {
    apply_conversion_overrides(&arguments, &mut loaded)?;
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

    let policy = ExecutionPolicy {
        options: loaded.options,
        hint: FormatHint {
            format: arguments.format.as_deref().map(parse_format).transpose()?,
            extension: arguments.extension,
            media_type: arguments.mime_type,
            charset: arguments.charset,
            ..FormatHint::default()
        },
        emit,
        asset_mode,
        conflict,
        assets_dir: arguments.assets_dir,
    };
    let reports = if plans.len() == 1 && plans[0].output.is_none() {
        vec![process_stdout(&plans[0], &policy, catalog, json_log, context)?]
    } else if plans.len() == 1 {
        let plan = &plans[0];
        let (output_path, diagnostics, warnings) = process_file_task_inner(plan, &policy)?;
        vec![BatchItemReport {
            input: plan.item.display.clone(),
            output: Some(output_path.display().to_string()),
            format: policy.hint.format.map(|format| format.as_str().into()),
            status: BatchItemStatus::Success,
            diagnostics: diagnostics.iter().map(Into::into).collect(),
            error_code: None,
            message: None,
            warnings,
        }]
    } else {
        process_batch(plans, &policy, arguments.jobs.map_or(loaded.jobs, std::num::NonZero::get))?
    };
    finish_reports(reports, arguments.report.as_deref(), global, catalog, json_log, context.stderr)
}

fn finish_reports(
    reports: Vec<BatchItemReport>,
    report_path: Option<&Path>,
    global: &crate::args::GlobalArgs,
    catalog: Catalog,
    json_log: bool,
    stderr: &mut dyn Write,
) -> Result<(), CliError> {
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
    let report = BatchReport::try_new(reports)
        .map_err(|error| CliError::internal(format!("build batch report DTO: {error}")))?;
    if let Some(path) = report_path {
        output::write_report(path, &report)?;
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
    let options = &mut loaded.options;
    if let Some(policy) = arguments.ocr {
        options.ocr.policy = match policy {
            OcrPolicyArg::Off => OcrPolicy::Off,
            OcrPolicyArg::Auto => OcrPolicy::Auto,
            OcrPolicyArg::Always => OcrPolicy::Always,
        };
    }
    if let Some(bundle) = &arguments.ocr_model {
        options.ocr.model_bundle = Some(bundle.clone());
    }
    if let Some(confidence) = arguments.ocr_min_confidence {
        config::validate_confidence(confidence)?;
        options.ocr.minimum_confidence = confidence;
    }
    if let Some(value) = arguments.max_redirects {
        options.network.max_redirects = value;
    }
    apply_network_authorization(
        options,
        arguments.allow_network,
        arguments.allow_private_network,
        &arguments.allow_host,
    )?;
    macro_rules! assign {
        ($argument:ident, $field:ident) => {
            if let Some(value) = arguments.$argument {
                options.limits.$field = value;
            }
        };
    }
    assign!(max_input_size, max_input_bytes);
    assign!(max_decompressed_size, max_decompressed_bytes);
    assign!(max_archive_entries, max_archive_entries);
    assign!(max_depth, max_nesting_depth);
    assign!(max_pages, max_pages);
    assign!(max_asset_size, max_asset_bytes);
    if let Some(mode) = arguments.asset_mode {
        options.output.asset_mode = match mode {
            AssetModeArg::Extract => AssetMode::Extract,
            AssetModeArg::Embed => AssetMode::Embed,
            AssetModeArg::Omit => AssetMode::Omit,
        };
    }
    for assignment in &arguments.ai {
        let (capability, mode) = split_assignment(assignment, "--ai")?;
        config::validate_capability(capability)?;
        let mode = parse_ai_mode(mode)?;
        set_ai_mode(options, capability, mode);
    }
    for prompt in &arguments.ai_prompt {
        let (capability, path) = split_assignment(prompt, "--ai-prompt")?;
        config::validate_capability(capability)?;
        if !Path::new(path).is_file() {
            return Err(CliError::usage(format!("AI prompt file does not exist: {path}")));
        }
        loaded.prompts.insert(capability.into(), PathBuf::from(path));
    }
    if !arguments.ocr_language.is_empty() {
        loaded.ocr_languages.clone_from(&arguments.ocr_language);
    }
    if let Some(provider) = &arguments.ai_provider {
        if !loaded.effective.providers.contains_key(provider) {
            return Err(CliError::usage(format!("unknown AI provider '{provider}'")));
        }
        loaded.ai_provider = Some(provider.clone());
    }
    if let Some(model) = &arguments.ai_model {
        loaded.ai_model = Some(model.clone());
    }
    if ai_is_enabled(options) {
        let provider_name = loaded.ai_provider.as_deref().ok_or_else(|| {
            CliError::usage(
                "an enabled AI capability requires --ai-provider or a configured default provider",
            )
        })?;
        let provider = loaded
            .effective
            .providers
            .get(provider_name)
            .ok_or_else(|| CliError::usage(format!("unknown AI provider '{provider_name}'")))?;
        validate_network_url(&provider.base_url, options, "AI provider")?;
    }
    Ok(())
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

fn ai_is_enabled(options: &ConversionOptions) -> bool {
    [
        options.ai.vision_ocr,
        options.ai.image_description,
        options.ai.layout_repair,
        options.ai.table_repair,
        options.ai.formula_repair,
        options.ai.audio_transcription,
        options.ai.markdown_postprocess,
    ]
    .iter()
    .any(|mode| *mode != AiMode::Off)
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
                from_directory: false,
                local_path: None,
            });
            continue;
        }
        let text = value.to_string_lossy();
        if is_uri(&text) {
            let parsed = url::Url::parse(&text)
                .map_err(|error| CliError::usage(format!("invalid input URI '{text}': {error}")))?;
            let name = parsed
                .path_segments()
                .and_then(Iterator::last)
                .filter(|name| !name.is_empty())
                .unwrap_or("remote-document");
            output.push(WorkItem {
                input: InputRef::Uri(text.into_owned()),
                display: value.to_string_lossy().into_owned(),
                relative: PathBuf::from(sanitize_component(name)),
                root_label: sanitize_component(parsed.host_str().unwrap_or("remote")),
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
    let collisions = plans.iter().filter_map(|plan| plan.output.as_ref()).fold(
        BTreeMap::<PathBuf, usize>::new(),
        |mut counts, path| {
            *counts.entry(path.clone()).or_default() += 1;
            counts
        },
    );
    for plan in plans.iter_mut() {
        let Some(path) = plan.output.as_mut() else {
            continue;
        };
        if collisions.get(path).copied().unwrap_or_default() > 1
            && let Some(root) = &plan.output_root
            && let Ok(relative) = path.strip_prefix(root)
        {
            *path = root.join(&plan.item.root_label).join(relative);
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
    catalog: Catalog,
    json_log: bool,
    context: &mut RunContext<'_>,
) -> Result<BatchItemReport, CliError> {
    let result = convert_item(&plan.item, policy)?;
    if policy.asset_mode == AssetModeArg::Extract && !result.assets.is_empty() {
        let assets_dir = policy
            .assets_dir
            .clone()
            .or_else(|| plan.item.local_path.as_deref().map(default_asset_directory));
        let assets_dir = assets_dir.ok_or_else(|| {
            CliError::usage("stdin and URI inputs with extracted assets require --assets-dir")
        })?;
        for outcome in
            output::write_assets(&result.assets, &assets_dir, policy.asset_mode, policy.conflict)?
        {
            if outcome.renamed {
                write_stderr_event(
                    context.stderr,
                    json_log,
                    "warning",
                    "assetRenamed",
                    &format!("asset output renamed to {}", outcome.path.display()),
                    Some(&plan.item.display),
                    &format!(
                        "{}: asset output renamed to {}",
                        catalog.warning_prefix(),
                        outcome.path.display()
                    ),
                )?;
            }
        }
    }
    context.stdout.write_all(&output::encode_result(&result, policy.emit)?)?;
    Ok(BatchItemReport {
        input: plan.item.display.clone(),
        output: None,
        format: policy.hint.format.map(|format| format.as_str().into()),
        status: BatchItemStatus::Success,
        diagnostics: result.diagnostics.iter().map(Into::into).collect(),
        error_code: None,
        message: None,
        warnings: vec![],
    })
}

fn process_batch(
    plans: Vec<WorkPlan>,
    policy: &ExecutionPolicy,
    jobs: usize,
) -> Result<Vec<BatchItemReport>, CliError> {
    let task_count = plans.len();
    let queue = Arc::new(Mutex::new(VecDeque::from(plans)));
    let (sender, receiver) = mpsc::channel();
    let worker_count = jobs.max(1).min(task_count.max(1));
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
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
                    let report = process_file_task(task, &policy);
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

fn process_file_task(plan: WorkPlan, policy: &ExecutionPolicy) -> BatchItemReport {
    match process_file_task_inner(&plan, policy) {
        Ok((output_path, diagnostics, warnings)) => BatchItemReport {
            input: plan.item.display,
            output: Some(output_path.display().to_string()),
            format: policy.hint.format.map(|format| format.as_str().into()),
            status: BatchItemStatus::Success,
            diagnostics: diagnostics.iter().map(Into::into).collect(),
            error_code: None,
            message: None,
            warnings,
        },
        Err(error) => BatchItemReport {
            input: plan.item.display,
            output: plan.output.map(|path| path.display().to_string()),
            format: policy.hint.format.map(|format| format.as_str().into()),
            status: BatchItemStatus::Failed,
            diagnostics: vec![],
            error_code: Some(error.code().into()),
            message: Some(error.to_string()),
            warnings: vec![],
        },
    }
}

fn process_file_task_inner(
    plan: &WorkPlan,
    policy: &ExecutionPolicy,
) -> Result<(PathBuf, Vec<into_markdown::Diagnostic>, Vec<String>), CliError> {
    let result = convert_item(&plan.item, policy)?;
    let requested =
        plan.output.as_deref().ok_or_else(|| CliError::internal("batch output path is absent"))?;
    let outcome = output::write_file(
        requested,
        &output::encode_result(&result, policy.emit)?,
        policy.conflict,
    )?;
    let mut warnings = Vec::new();
    if outcome.renamed {
        warnings.push(format!(
            "output renamed to {} because the requested path existed",
            outcome.path.display()
        ));
    }
    if policy.asset_mode == AssetModeArg::Extract && !result.assets.is_empty() {
        let assets_dir =
            policy.assets_dir.clone().unwrap_or_else(|| default_asset_directory(&outcome.path));
        for asset in
            output::write_assets(&result.assets, &assets_dir, policy.asset_mode, policy.conflict)?
        {
            if asset.renamed {
                warnings.push(format!("asset renamed to {}", asset.path.display()));
            }
        }
    }
    Ok((outcome.path, result.diagnostics, warnings))
}

fn convert_item(
    item: &WorkItem,
    policy: &ExecutionPolicy,
) -> Result<into_markdown::ConversionResult, CliError> {
    validate_input_network(&item.input, &policy.options)?;
    let mut request = ConversionRequest::new(item.input.clone());
    request.options = policy.options.clone();
    request.hint = policy.hint.clone();
    let engine = into_markdown::default_engine().map_err(CliError::from)?;
    futures::executor::block_on(engine.convert(request)).map_err(CliError::from)
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
        .map(|descriptor| descriptor.format)
        .ok_or_else(|| CliError::usage(format!("unknown format '{value}'")))
}

fn find_format(value: &str) -> Option<&'static into_markdown::FormatDescriptor> {
    let normalized = value.trim_start_matches('.').to_ascii_lowercase();
    into_markdown::planned_formats().iter().find(|descriptor| {
        descriptor.format.as_str() == normalized
            || descriptor.extensions.iter().any(|extension| *extension == normalized)
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
    if let Ok(mut parsed) = url::Url::parse(value) {
        parsed.set_query(None);
        parsed.set_fragment(None);
        parsed.to_string()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn invoke(arguments: &[&str], stdin_is_terminal: bool) -> Result<(String, String), CliError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let root = std::env::temp_dir().join(format!(
            "into-md-app-test-{}-{}",
            std::process::id(),
            arguments.join("-").replace('/', "_")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let result = run(
            arguments.iter().map(OsString::from).collect(),
            RunContext {
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

    #[test]
    fn terminal_without_input_prints_help() {
        let (stdout, _) = invoke(&[], true).unwrap();
        assert!(stdout.contains("Usage:"));
        assert!(stdout.contains("providers"));
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
        assert!(version.contains("\"name\": \"into-md\""));
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
        assert_eq!(lines[2], "docx\t0.550\tfalse\tbuiltin.detector.hints\tfilename extension\t");
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
    fn unavailable_management_backend_has_exit_nine() {
        let error = invoke(&["models", "install"], true).unwrap_err();
        assert_eq!(error.exit_code(), 9);
        assert_eq!(error.code(), "componentUnavailable");
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
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 3);
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
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();
        assert!(String::from_utf8(stdout).unwrap().contains("sub/a.md"));
        assert!(!output.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn colliding_input_roots_receive_stable_root_prefixes() {
        let output = PathBuf::from("out");
        let item = |root_label: &str, display: &str| WorkItem {
            input: InputRef::Path(PathBuf::from(display)),
            display: display.into(),
            relative: PathBuf::from("same.pdf"),
            root_label: root_label.into(),
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
[providers.remote]
type = "openai-compatible"
base_url = "https://other.example/v1"
model = "vision"
api_key_env = "REMOTE_KEY"
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
    fn sha256_is_available_for_future_local_plugin_records() {
        let digest = Sha256::digest(b"plugin");
        assert_eq!(format!("{digest:x}").len(), 64);
    }

    #[test]
    fn reserved_command_filename_can_be_passed_after_double_dash() {
        let parsed = Cli::try_parse_from(["into-md", "--", "formats"]).unwrap();
        assert!(parsed.command.is_none());
        assert_eq!(parsed.conversion.inputs, vec![OsString::from("formats")]);
    }
}
