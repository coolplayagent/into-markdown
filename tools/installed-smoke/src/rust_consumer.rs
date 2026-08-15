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
    resolve: CargoResolve,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    source: Option<String>,
    manifest_path: String,
}

#[derive(Deserialize)]
struct CargoResolve {
    root: Option<String>,
    nodes: Vec<CargoNode>,
}

#[derive(Deserialize)]
struct CargoNode {
    id: String,
    dependencies: Vec<String>,
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
    configure_macos_linker(&mut environment)?;
    let invocation_root = filesystem_root(&home)?;
    validate_installed_metadata(request, &invocation_root, &home, &environment, executor)?;
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
            current_dir: &invocation_root,
            home: &home,
            environment: environment.clone(),
            stdin: &[],
            timeout: request.timeout.saturating_mul(4),
            cancel_file: request.cancel_file.as_deref(),
        })?;
        if output.exit_code != Some(0) {
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            let tail = diagnostics.char_indices().rev().nth(180).map_or(0, |(index, _)| index);
            return Err(format!(
                "offline external consumer compilation failed (exit {:?}): {}",
                output.exit_code,
                diagnostics[tail..].trim()
            ));
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

fn configure_macos_linker(environment: &mut BTreeMap<String, String>) -> Result<(), String> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let linker = Path::new("/usr/bin/clang")
            .canonicalize()
            .map_err(|_| "fixed macOS linker is unavailable".to_owned())?;
        if !linker.is_file() {
            return Err("fixed macOS linker is not a regular file".into());
        }
        environment.insert(
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".into(),
            linker.display().to_string(),
        );
    }
    Ok(())
}

fn validate_installed_metadata(
    request: &ValidatedRequest,
    invocation_root: &Path,
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
        // from a manifest passed explicitly. Invoke at a configuration-free
        // filesystem root so neither installed nor mutable ancestor config can
        // add wrappers or development sources.
        current_dir: invocation_root,
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
    validate_metadata_authority(request, &metadata)
}

fn validate_metadata_authority(
    request: &ValidatedRequest,
    metadata: &CargoMetadata,
) -> Result<(), String> {
    let requested_manifest = request
        .rust_library
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|_| "cannot resolve installed Rust manifest".to_owned())?;
    let vendor = request
        .rust_library
        .join("vendor")
        .canonicalize()
        .map_err(|_| "cannot resolve installed vendor directory".to_owned())?;
    let package_ids = metadata
        .packages
        .iter()
        .map(|package| package.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let node_ids = metadata
        .resolve
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if package_ids.len() != metadata.packages.len()
        || node_ids.len() != metadata.resolve.nodes.len()
        || package_ids != node_ids
        || metadata
            .resolve
            .nodes
            .iter()
            .flat_map(|node| &node.dependencies)
            .any(|id| !package_ids.contains(id.as_str()))
    {
        return Err("installed Rust dependency closure is not exact".into());
    }
    let root_id = metadata
        .resolve
        .root
        .as_deref()
        .ok_or_else(|| "installed Rust dependency closure has no root".to_owned())?;
    if !package_ids.contains(root_id) {
        return Err("installed Rust dependency closure root is unknown".into());
    }
    let nodes = metadata
        .resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut reachable = std::collections::BTreeSet::new();
    let mut pending = vec![root_id];
    while let Some(id) = pending.pop() {
        if !reachable.insert(id) {
            continue;
        }
        let node = nodes
            .get(id)
            .ok_or_else(|| "installed Rust dependency closure node is unknown".to_owned())?;
        pending.extend(node.dependencies.iter().map(String::as_str));
    }
    if reachable != package_ids {
        return Err("installed Rust dependency closure contains unreachable packages".into());
    }
    let mut facade = 0_u8;
    let mut facade_id = None;
    for package in &metadata.packages {
        let manifest = Path::new(&package.manifest_path)
            .canonicalize()
            .map_err(|_| "cannot resolve installed dependency manifest".to_owned())?;
        match package.source.as_deref() {
            None if !manifest.starts_with(&request.rust_library) => {
                return Err("installed Rust package references a path dependency outside the installed library".into());
            }
            Some(source) if source.starts_with("registry+") && !manifest.starts_with(&vendor) => {
                return Err("installed registry package is not resolved from the installed vendor directory".into());
            }
            Some(source) if !source.starts_with("registry+") => {
                return Err("installed Rust package uses a forbidden non-registry source".into());
            }
            _ => {}
        }
        if package.name == "into-markdown" && manifest == requested_manifest {
            facade = facade.saturating_add(1);
            facade_id = Some(package.id.as_str());
        }
    }
    if facade != 1 || facade_id != Some(root_id) {
        return Err("installed Rust package root is not the unique into-markdown facade".into());
    }
    Ok(())
}

fn filesystem_root(path: &Path) -> Result<std::path::PathBuf, String> {
    let canonical =
        path.canonicalize().map_err(|_| "cannot resolve Cargo isolation path".to_owned())?;
    let root = canonical
        .ancestors()
        .last()
        .map(Path::to_owned)
        .ok_or_else(|| "cannot identify Cargo filesystem root".to_owned())?;
    if [root.join(".cargo/config.toml"), root.join(".cargo/config")]
        .iter()
        .any(|candidate| candidate.exists())
    {
        return Err("Cargo filesystem root contains an inherited configuration".into());
    }
    Ok(root)
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
    #[allow(clippy::too_many_lines)]
    fn metadata_rejects_development_path_dependency() {
        let temporary = tempfile::tempdir().unwrap();
        let install = temporary.path().join("install");
        let library = install.join("lib/into-markdown-rust");
        let external = temporary.path().join("developer-source");
        fs::create_dir_all(&library).unwrap();
        fs::create_dir_all(library.join("vendor")).unwrap();
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
            timeout: Duration::from_secs(1),
            cancel_file: None,
        };
        let metadata = serde_json::json!({
            "packages": [
                {"id":"root","name": "into-markdown", "source": null, "manifest_path": library.join("Cargo.toml")},
                {"id":"stolen","name": "stolen", "source": null, "manifest_path": external.join("Cargo.toml")}
            ],
            "resolve":{"root":"root","nodes":[
                {"id":"root","dependencies":["stolen"]},
                {"id":"stolen","dependencies":[]}
            ]}
        });
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: serde_json::to_vec(&metadata).unwrap(),
            stderr: vec![],
        })));
        let error = validate_installed_metadata(
            &request,
            filesystem_root(temporary.path()).unwrap().as_path(),
            temporary.path(),
            &BTreeMap::new(),
            &executor,
        )
        .unwrap_err();
        assert!(error.contains("outside the installed library"));

        let registry_cache = external.join("registry/Cargo.toml");
        fs::create_dir_all(registry_cache.parent().unwrap()).unwrap();
        fs::write(&registry_cache, b"registry").unwrap();
        let metadata = serde_json::json!({
            "packages": [
                {"id":"root","name":"into-markdown","source":null,"manifest_path":library.join("Cargo.toml")},
                {"id":"registry","name":"registry","source":"registry+https://example.invalid/index","manifest_path":registry_cache}
            ],
            "resolve":{"root":"root","nodes":[
                {"id":"root","dependencies":["registry"]},
                {"id":"registry","dependencies":[]}
            ]}
        });
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: serde_json::to_vec(&metadata).unwrap(),
            stderr: vec![],
        })));
        let error = validate_installed_metadata(
            &request,
            filesystem_root(temporary.path()).unwrap().as_path(),
            temporary.path(),
            &BTreeMap::new(),
            &executor,
        )
        .unwrap_err();
        assert!(error.contains("not resolved from the installed vendor"));

        let vendored_manifest = library.join("vendor/pkg/Cargo.toml");
        fs::create_dir_all(vendored_manifest.parent().unwrap()).unwrap();
        fs::write(&vendored_manifest, b"git").unwrap();
        let metadata = serde_json::json!({
            "packages": [
                {"id":"root","name":"into-markdown","source":null,"manifest_path":library.join("Cargo.toml")},
                {"id":"git","name":"git","source":"git+https://example.invalid/repo","manifest_path":vendored_manifest}
            ],
            "resolve":{"root":"root","nodes":[
                {"id":"root","dependencies":["git"]},
                {"id":"git","dependencies":[]}
            ]}
        });
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: serde_json::to_vec(&metadata).unwrap(),
            stderr: vec![],
        })));
        let error = validate_installed_metadata(
            &request,
            filesystem_root(temporary.path()).unwrap().as_path(),
            temporary.path(),
            &BTreeMap::new(),
            &executor,
        )
        .unwrap_err();
        assert!(error.contains("forbidden non-registry source"));

        let metadata = serde_json::json!({
            "packages": [
                {"id":"root","name":"into-markdown","source":null,"manifest_path":library.join("Cargo.toml")},
                {"id":"orphan","name":"orphan","source":"registry+https://example.invalid/index","manifest_path":library.join("vendor/pkg/Cargo.toml")}
            ],
            "resolve":{"root":"root","nodes":[
                {"id":"root","dependencies":[]},
                {"id":"orphan","dependencies":[]}
            ]}
        });
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: serde_json::to_vec(&metadata).unwrap(),
            stderr: vec![],
        })));
        let error = validate_installed_metadata(
            &request,
            filesystem_root(temporary.path()).unwrap().as_path(),
            temporary.path(),
            &BTreeMap::new(),
            &executor,
        )
        .unwrap_err();
        assert!(error.contains("unreachable packages"));
    }

    #[cfg(unix)]
    #[test]
    fn cargo_does_not_read_malicious_temp_ancestor_wrapper() {
        use crate::process::RealExecutor;
        use std::os::unix::fs::PermissionsExt;

        let Some(cargo) = option_env!("CARGO") else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(temporary.path().join(".cargo")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            b"[package]\nname='isolated-probe'\nversion='0.0.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(project.join("src/main.rs"), b"fn main() {}\n").unwrap();
        let marker = temporary.path().join("wrapper-ran");
        let wrapper = temporary.path().join("malicious-wrapper.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf owned > '{}'
exec \"$@\"\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            temporary.path().join(".cargo/config.toml"),
            format!("[build]\nrustc-wrapper = {:?}\n", wrapper.display().to_string()),
        )
        .unwrap();
        let home = prepare_home(temporary.path(), "isolated-home").unwrap();
        let cargo_home = home.join("cargo-home");
        fs::create_dir(&cargo_home).unwrap();
        fs::write(cargo_home.join("config.toml"), "[net]\noffline=true\n").unwrap();
        let mut environment = command_environment(&home);
        environment.insert("CARGO_HOME".into(), cargo_home.display().to_string());
        environment.insert("CARGO_TARGET_DIR".into(), home.join("target").display().to_string());
        let sysroot =
            std::process::Command::new("rustc").args(["--print", "sysroot"]).output().unwrap();
        assert!(sysroot.status.success());
        let rustc =
            Path::new(std::str::from_utf8(&sysroot.stdout).unwrap().trim()).join("bin/rustc");
        environment.insert("RUSTC".into(), rustc.display().to_string());
        let arguments = vec![
            "check".into(),
            "--offline".into(),
            "--manifest-path".into(),
            project.join("Cargo.toml").display().to_string(),
        ];
        let output = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: Path::new(cargo),
                arguments: &arguments,
                current_dir: &filesystem_root(temporary.path()).unwrap(),
                home: &home,
                environment,
                stdin: &[],
                timeout: Duration::from_secs(20),
                cancel_file: None,
            })
            .unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!marker.exists());
    }
}
