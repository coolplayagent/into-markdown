//! Command-line adapter for the installed smoke contract.

use clap::Parser;
use installed_smoke::{SmokeRequest, run};
use std::process::ExitCode;

fn main() -> ExitCode {
    let request = SmokeRequest::parse();
    match run(request) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("installed-smoke: {error}");
            ExitCode::FAILURE
        }
    }
}
