//! Command-line entry point for offline license governance checks.

use std::process::ExitCode;

fn main() -> ExitCode {
    let default_mode = option_env!("LICENSE_CHECK_DEFAULT_MODE").unwrap_or("check");
    let mode = std::env::args().nth(1).unwrap_or_else(|| default_mode.to_owned());
    let release = match mode.as_str() {
        "check" => false,
        "release" => true,
        _ => {
            eprintln!("usage: license-check [check|release]");
            return ExitCode::from(2);
        }
    };

    match license_check::run(release) {
        Ok(()) => {
            println!("license audit passed ({mode})");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for error in errors {
                eprintln!("license audit: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
