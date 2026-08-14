//! External Rust consumer compiled exclusively from installed sources.

use crate::process::{CommandSpec, Executor, command_environment, prepare_home};
use crate::report::CaseResult;
use crate::request::ValidatedRequest;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    source: Option<String>,
    manifest_path: String,
}

pub(crate) fn run(
    request: &ValidatedRequest,
    root: &Path,
    executor: &dyn Executor,
    cases: &mut Vec<CaseResult>,
) {
    let result = compile_and_run(request, root, executor);
    cases.push(match result {
        Ok(()) => CaseResult::passed(
            "rust-external-consumer",
            "offline installed library linked and converted memory DTO",
        ),
        Err(error) => CaseResult::failed("rust-external-consumer", "consumerFailed", &error),
    });
}

fn compile_and_run(
    request: &ValidatedRequest,
    root: &Path,
    executor: &dyn Executor,
) -> Result<(), String> {
    let consumer = root.join("external-consumer");
    let source = consumer.join("src");
    fs::create_dir_all(&source)
        .map_err(|error| format!("cannot create external consumer: {error}"))?;
    let library = toml_path(&request.rust_library)?;
    fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\nname = \"installed-smoke-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\ninto-markdown = {{ path = {library:?} }}\n"
        ),
    )
    .map_err(|error| format!("cannot write external consumer manifest: {error}"))?;
    fs::write(source.join("main.rs"), CONSUMER_SOURCE)
        .map_err(|error| format!("cannot write external consumer source: {error}"))?;

    let home = prepare_home(root, "rust-home")?;
    let cargo_home = home.join("cargo-home");
    fs::create_dir(&cargo_home)
        .map_err(|error| format!("cannot create empty Cargo home: {error}"))?;
    let vendor = toml_path(&request.rust_library.join("vendor"))?;
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[net]\noffline = true\n\n[source.crates-io]\nreplace-with = \"installed-vendor\"\n\n[source.installed-vendor]\ndirectory = {vendor:?}\n"
        ),
    )
    .map_err(|error| format!("cannot write isolated Cargo config: {error}"))?;
    let target = root.join("consumer-target");
    let mut environment = command_environment(&home);
    environment.extend(BTreeMap::from([
        ("CARGO_HOME".into(), cargo_home.display().to_string()),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
        ("CARGO_TARGET_DIR".into(), target.display().to_string()),
        ("RUSTC".into(), request.rustc.display().to_string()),
    ]));
    validate_installed_metadata(request, &home, &environment, executor)?;
    let manifest = consumer.join("Cargo.toml").display().to_string();
    for arguments in [
        vec![
            "generate-lockfile".into(),
            "--offline".into(),
            "--manifest-path".into(),
            manifest.clone(),
        ],
        vec![
            "build".into(),
            "--offline".into(),
            "--locked".into(),
            "--manifest-path".into(),
            manifest,
        ],
    ] {
        let output = executor.execute(CommandSpec {
            program: &request.cargo,
            arguments: &arguments,
            current_dir: &consumer,
            home: &home,
            environment: environment.clone(),
            stdin: &[],
            timeout: request.timeout.saturating_mul(4),
            cancel_file: request.cancel_file.as_deref(),
        })?;
        if output.exit_code != Some(0) {
            return Err("offline external consumer compilation failed".into());
        }
    }
    let executable = target
        .join("debug")
        .join(format!("installed-smoke-consumer{}", std::env::consts::EXE_SUFFIX));
    let arguments = Vec::new();
    let output = executor.execute(CommandSpec {
        program: &executable,
        arguments: &arguments,
        current_dir: &consumer,
        home: &home,
        environment: command_environment(&home),
        stdin: &[],
        timeout: request.timeout,
        cancel_file: request.cancel_file.as_deref(),
    })?;
    let dto: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "external consumer output is not a DTO".to_owned())?;
    if output.exit_code == Some(0)
        && output.stderr.is_empty()
        && dto["schemaVersion"] == 1
        && dto["markdown"] == "Installed Rust consumer\n"
    {
        Ok(())
    } else {
        Err("external consumer DTO contract failed".into())
    }
}

fn validate_installed_metadata(
    request: &ValidatedRequest,
    home: &Path,
    environment: &BTreeMap<String, String>,
    executor: &dyn Executor,
) -> Result<(), String> {
    let arguments = vec![
        "metadata".into(),
        "--offline".into(),
        "--locked".into(),
        "--format-version".into(),
        "1".into(),
        "--manifest-path".into(),
        request.rust_library.join("Cargo.toml").display().to_string(),
    ];
    let output = executor.execute(CommandSpec {
        program: &request.cargo,
        arguments: &arguments,
        // Cargo discovers configuration from the invocation directory, not
        // from a manifest passed explicitly. Keep this in the isolated run
        // root so an installed `.cargo/config.toml` cannot add tool wrappers
        // or development sources outside the authority checked below.
        current_dir: home,
        home,
        environment: environment.clone(),
        stdin: &[],
        timeout: request.timeout.saturating_mul(2),
        cancel_file: request.cancel_file.as_deref(),
    })?;
    if output.exit_code != Some(0) {
        return Err("installed Rust package metadata failed offline".into());
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|_| "installed Rust package metadata is invalid".to_owned())?;
    let requested_manifest = request
        .rust_library
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|_| "cannot resolve installed Rust manifest".to_owned())?;
    let mut facade = 0_u8;
    for package in metadata.packages.iter().filter(|package| package.source.is_none()) {
        let manifest = Path::new(&package.manifest_path)
            .canonicalize()
            .map_err(|_| "cannot resolve installed path dependency".to_owned())?;
        if !manifest.starts_with(&request.install_root) {
            return Err(
                "installed Rust package references a local dependency outside installation".into(),
            );
        }
        if package.name == "into-markdown" && manifest == requested_manifest {
            facade = facade.saturating_add(1);
        }
    }
    if facade != 1 {
        return Err("installed Rust package root is not the unique into-markdown facade".into());
    }
    Ok(())
}

fn toml_path(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(|value| value.replace('\\', "\\\\"))
        .ok_or_else(|| "installed Rust package path is not Unicode".to_owned())
}

const CONSUMER_SOURCE: &str = r#"use into_markdown::{ConversionRequest, DtoJsonStyle, InputRef, ResultDto, default_engine};
use std::future::Future;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn main() {
    let engine = default_engine().expect("installed engine");
    let request = ConversionRequest::new(InputRef::bytes(
        b"Installed Rust consumer\n".as_slice(),
        Some("memory.txt"),
    ));
    let result = block_on(engine.convert(request)).expect("memory conversion");
    let json = ResultDto::json_from_result(&result, DtoJsonStyle::Compact).expect("stable DTO");
    println!("{json}");
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::CommandOutput;
    use std::sync::Mutex;
    use std::time::Duration;

    struct FakeExecutor(Mutex<Option<CommandOutput>>);

    impl Executor for FakeExecutor {
        fn execute(&self, _: CommandSpec<'_>) -> Result<CommandOutput, String> {
            self.0.lock().unwrap().take().ok_or_else(|| "unexpected command".into())
        }
    }

    #[test]
    fn metadata_rejects_development_path_dependency() {
        let temporary = tempfile::tempdir().unwrap();
        let install = temporary.path().join("install");
        let library = install.join("lib/into-markdown-rust");
        let external = temporary.path().join("developer-source");
        fs::create_dir_all(&library).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(library.join("Cargo.toml"), b"package").unwrap();
        fs::write(external.join("Cargo.toml"), b"dependency").unwrap();
        let executable = install.join("cargo");
        fs::write(&executable, b"cargo").unwrap();
        let request = ValidatedRequest {
            install_root: install.canonicalize().unwrap(),
            into_md: executable.clone(),
            rust_library: library.canonicalize().unwrap(),
            manifest: executable.clone(),
            fixtures: install.clone(),
            temp_root: temporary.path().to_owned(),
            report: temporary.path().join("report"),
            archive_sha256: "a".repeat(64),
            cargo: executable.clone(),
            rustc: executable,
            pdfium_library: None,
            timeout: Duration::from_secs(1),
            cancel_file: None,
        };
        let metadata = serde_json::json!({
            "packages": [
                {"name": "into-markdown", "source": null, "manifest_path": library.join("Cargo.toml")},
                {"name": "stolen", "source": null, "manifest_path": external.join("Cargo.toml")}
            ]
        });
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: serde_json::to_vec(&metadata).unwrap(),
            stderr: vec![],
        })));
        let error =
            validate_installed_metadata(&request, temporary.path(), &BTreeMap::new(), &executor)
                .unwrap_err();
        assert!(error.contains("outside installation"));
    }
}
