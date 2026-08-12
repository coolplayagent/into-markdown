//! Strict release-audit entry point; its mode cannot be changed by arguments.

use std::process::ExitCode;

fn main() -> ExitCode {
    license_check::main_for_mode(true)
}
