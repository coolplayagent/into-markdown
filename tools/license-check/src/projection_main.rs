//! Narrow command-line adapter for release input generation and archive projection verification.

use license_check::release::{generate_release_inputs, verify_archive_projection};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for error in errors {
                eprintln!("release-projection: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<String, Vec<String>> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.len() != 2 {
        return Err(vec![
            "usage: release-projection <generate|verify> <projection.json>".to_owned(),
        ]);
    }
    let operation = arguments[0].to_string_lossy();
    let input_path = PathBuf::from(&arguments[1]);
    let input = fs::read_to_string(&input_path)
        .map_err(|error| vec![format!("cannot read {}: {error}", input_path.display())])?;
    let root = repository_root()?;
    match operation.as_ref() {
        "generate" => generate_release_inputs(&root, &input).and_then(|inputs| {
            serde_json::to_string_pretty(&inputs)
                .map_err(|error| vec![format!("cannot serialize release inputs: {error}")])
        }),
        "verify" => verify_archive_projection(&root, &input)
            .map(|()| "archive projection passed".to_owned()),
        _ => Err(vec![format!("unknown operation {operation:?}")]),
    }
}

fn repository_root() -> Result<PathBuf, Vec<String>> {
    if let Ok(workspace) = env::var("BUILD_WORKSPACE_DIRECTORY") {
        let root = PathBuf::from(workspace)
            .canonicalize()
            .map_err(|error| vec![format!("cannot resolve Bazel workspace: {error}")])?;
        if root.join("Cargo.lock").is_file() && root.join("MODULE.bazel").is_file() {
            return Ok(root);
        }
        return Err(vec!["BUILD_WORKSPACE_DIRECTORY is not the repository root".to_owned()]);
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = manifest.parent().and_then(Path::parent) else {
        return Err(vec!["cannot resolve repository root".to_owned()]);
    };
    Ok(root.to_owned())
}
