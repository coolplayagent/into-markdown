//! `into-md` command-line application.

mod admin;
mod app;
mod args;
mod config;
mod error;
mod i18n;
mod output;
mod proxy_env;
mod services;
mod transaction;
mod ui;
mod ui_assets;
mod web_tasks;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};

fn main() {
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
        let event = serde_json::json!({
            "code": error.code(),
            "message": error.message(),
            "exitCode": error.exit_code(),
        });
        writeln!(stderr, "{event}")
    } else {
        writeln!(stderr, "{}: {}", catalog.error_prefix(), error)
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
    }
}
