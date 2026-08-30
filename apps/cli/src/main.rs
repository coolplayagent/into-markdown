//! `into-md` command-line application.

mod admin;
mod app;
mod args;
mod config;
mod embedded_runtime;
mod error;
mod i18n;
mod output;
mod proxy_env;
mod result_policy;
mod services;
mod timing;
mod transaction;
mod ui;
mod ui_assets;
mod web_tasks;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};

fn main() {
    // Registration installs only a function pointer. `-h`, `version`, and all
    // non-PDF conversions remain free of runtime-cache filesystem activity.
    embedded_runtime::register_pdfium_resolver();
    let arguments = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    let stdin_is_terminal = std::io::stdin().is_terminal();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(stderr, "into-md: failed to determine current directory: {error}");
            std::process::exit(error::ExitClass::Io.code());
        }
    };
    let language = i18n::requested_language(&arguments);
    let explicit_json_log = i18n::requested_json_log(&arguments);
    let result = app::run(
        arguments,
        app::RunContext {
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal,
            cwd,
            #[cfg(test)]
            user_data_anchor: None,
        },
    );
    // `app` retains verified process-plugin snapshots in a process-wide cache so one Web/CLI
    // invocation never recopies a 500+ MiB speech runtime. Static `OnceLock` contents are not
    // dropped automatically at process exit, so release them explicitly after all work stops.
    app::release_process_snapshots();
    embedded_runtime::release_temporary_runtimes();
    if let Err(error) = result {
        if error.is_broken_pipe() {
            return;
        }
        let catalog = i18n::Catalog::new(error.language().unwrap_or(language));
        let _ =
            write_error(&mut stderr, &error, catalog, error.uses_json_log() || explicit_json_log);
        std::process::exit(error.exit_code());
    }
}

fn write_error(
    stderr: &mut dyn Write,
    error: &error::CliError,
    catalog: i18n::Catalog,
    json_log: bool,
) -> std::io::Result<()> {
    if json_log {
        let mut event = serde_json::json!({
            "code": error.code(),
            "message": error.message(),
            "exitCode": error.exit_code(),
        });
        if let Some(duration_ms) = error.duration_ms() {
            event["durationMs"] = duration_ms.into();
        }
        if let Some(duration_ms) = error.processing_duration_ms() {
            event["processingDurationMs"] = duration_ms.into();
        }
        if let Some(duration_ms) = error.wall_duration_ms() {
            event["wallDurationMs"] = duration_ms.into();
        }
        writeln!(stderr, "{event}")
    } else {
        writeln!(stderr, "{}: {}", catalog.error_prefix(), error)?;
        if let Some(duration_ms) = error.duration_ms() {
            let processing = error.processing_duration_ms().map_or_else(
                || catalog.unavailable_label().to_owned(),
                |value| format!("{value:.2} ms"),
            );
            writeln!(
                stderr,
                "{}: {} {duration_ms:.2} ms, {} {processing}",
                catalog.timing_prefix(),
                catalog.total_duration_label(),
                catalog.processing_duration_label(),
            )?;
        }
        if let Some(duration_ms) = error.wall_duration_ms() {
            writeln!(
                stderr,
                "{}: {} {duration_ms:.2} ms",
                catalog.timing_prefix(),
                catalog.batch_wall_duration_label(),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_errors_have_stable_fields() {
        let error = error::CliError::component("model runtime missing");
        let mut bytes = Vec::new();
        write_error(&mut bytes, &error, i18n::Catalog::new(args::Language::En), true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["code"], "componentUnavailable");
        assert_eq!(value["exitCode"], 9);
        assert_eq!(value["message"], "model runtime missing");
        assert!(value.get("durationMs").is_none());
        assert!(value.get("processingDurationMs").is_none());
        assert!(value.get("wallDurationMs").is_none());
    }

    #[test]
    fn json_errors_add_timing_fields_when_one_execution_started() {
        let error = error::CliError::component("late output failure")
            .with_duration(12.5)
            .with_processing_duration(Some(8.25))
            .with_wall_duration(14.0);
        let mut bytes = Vec::new();
        write_error(&mut bytes, &error, i18n::Catalog::new(args::Language::En), true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["durationMs"], 12.5);
        assert_eq!(value["processingDurationMs"], 8.25);
        assert_eq!(value["wallDurationMs"], 14.0);
    }
}
