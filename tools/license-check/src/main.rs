//! Command-line entry point for offline license governance checks.

use std::process::ExitCode;

fn main() -> ExitCode {
    license_check::main_for_mode(false)
}
