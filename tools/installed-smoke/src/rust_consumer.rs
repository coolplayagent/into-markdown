//! External Rust consumer compiled exclusively from installed sources.

use crate::process::{CommandSpec, Executor, command_environment, prepare_home};
use crate::report::CaseResult;
use crate::request::ValidatedRequest;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
    target: &str,
    executor: &dyn Executor,
    cases: &mut Vec<CaseResult>,
) {
    let result = compile_and_run(request, root, target, executor);
    cases.push(match result {
        Ok(()) => CaseResult::passed(
            "rust-external-consumer",
            "offline installed library linked and converted text plus DOC/PPT/XLS DTOs",
        ),
        Err(error) => CaseResult::failed("rust-external-consumer", "consumerFailed", &error),
    });
}

fn compile_and_run(
    request: &ValidatedRequest,
    root: &Path,
    platform_target: &str,
    executor: &dyn Executor,
) -> Result<(), String> {
    let consumer = root.join("c");
    let source = consumer.join("src");
    fs::create_dir_all(&source)
        .map_err(|error| format!("cannot create external consumer: {error}"))?;

    let home = prepare_home(root, "h")?;
    let cargo_home = home.join("cargo");
    fs::create_dir(&cargo_home)
        .map_err(|error| format!("cannot create empty Cargo home: {error}"))?;
    write_cargo_config(&cargo_home, &request.rust_library.join("vendor"))?;
    let target = root.join("o");
    let mut environment = command_environment(&home);
    environment.extend(BTreeMap::from([
        ("CARGO_HOME".into(), cargo_path(&cargo_home)?),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
        ("CARGO_TARGET_DIR".into(), cargo_path(&target)?),
        ("RUSTC".into(), cargo_path(&request.rustc)?),
    ]));
    configure_linux_linker(platform_target, &mut environment)?;
    configure_macos_linker(&mut environment)?;
    configure_windows_linker(platform_target, &mut environment)?;
    let invocation_root = filesystem_root(&home)?;
    validate_installed_metadata(request, &invocation_root, &home, &environment, executor)?;

    // MSVC still applies legacy path limits while resolving relative includes in
    // vendored C sources. Compile an exact private snapshot under the bounded
    // runner root so a valid long or non-ASCII installation path remains usable.
    let library_snapshot = root.join("r");
    snapshot_installed_library(&request.rust_library, &library_snapshot)?;
    let library = cargo_path(&library_snapshot)?;
    fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\nname = \"installed-smoke-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\ninto-markdown = {{ path = {library:?} }}\n"
        ),
    )
    .map_err(|error| format!("cannot write external consumer manifest: {error}"))?;
    fs::write(source.join("main.rs"), CONSUMER_SOURCE)
        .map_err(|error| format!("cannot write external consumer source: {error}"))?;
    write_cargo_config(&cargo_home, &library_snapshot.join("vendor"))?;
    let manifest = cargo_path(&consumer.join("Cargo.toml"))?;
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
            let tail = diagnostics.char_indices().rev().nth(4_096).map_or(0, |(index, _)| index);
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
    let arguments = vec![request.fixtures.join("legacy").display().to_string()];
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

fn write_cargo_config(cargo_home: &Path, vendor: &Path) -> Result<(), String> {
    let vendor = cargo_path(vendor)?;
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[net]\noffline = true\n\n[source.crates-io]\nreplace-with = \"installed-vendor\"\n\n[source.installed-vendor]\ndirectory = {vendor:?}\n"
        ),
    )
    .map_err(|error| format!("cannot write isolated Cargo config: {error}"))
}

fn snapshot_installed_library(source: &Path, destination: &Path) -> Result<(), String> {
    prepare_snapshot_root(destination)?;
    let mut pending = vec![(source.to_owned(), destination.to_owned())];
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    while let Some((source_directory, destination_directory)) = pending.pop() {
        let mut entries = fs::read_dir(&source_directory)
            .map_err(|error| format!("cannot read installed Rust package: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot read installed Rust package: {error}"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let source_path = entry.path();
            let destination_path = destination_directory.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source_path)
                .map_err(|error| format!("cannot inspect installed Rust package: {error}"))?;
            if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
                return Err(
                    "installed Rust package snapshot contains a link or reparse point".into()
                );
            }
            if metadata.is_dir() {
                fs::create_dir(&destination_path)
                    .map_err(|error| format!("cannot create Rust package snapshot: {error}"))?;
                pending.push((source_path, destination_path));
            } else if metadata.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
                if files > 25_000 || bytes > 2 * 1024 * 1024 * 1024 {
                    return Err("installed Rust package snapshot exceeds its fixed budget".into());
                }
                let copied = fs::copy(&source_path, &destination_path)
                    .map_err(|error| format!("cannot copy installed Rust package: {error}"))?;
                if copied != metadata.len() {
                    return Err("installed Rust package snapshot copy is incomplete".into());
                }
            } else {
                return Err(
                    "installed Rust package contains an unsupported filesystem object".into()
                );
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_snapshot_root(path: &Path) -> Result<(), String> {
    into_markdown_process_plugin::create_windows_plugin_store_directory(path)
        .map_err(|error| format!("cannot prepare private Rust package snapshot: {error}"))
}

#[cfg(unix)]
fn prepare_snapshot_root(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir(path)
        .map_err(|error| format!("cannot prepare private Rust package snapshot: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot protect Rust package snapshot: {error}"))
}

#[cfg(not(any(unix, windows)))]
fn prepare_snapshot_root(path: &Path) -> Result<(), String> {
    fs::create_dir(path)
        .map_err(|error| format!("cannot prepare private Rust package snapshot: {error}"))
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn configure_macos_linker(environment: &mut BTreeMap<String, String>) -> Result<(), String> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let linker = Path::new("/usr/bin/clang")
            .canonicalize()
            .map_err(|_| "fixed macOS linker is unavailable".to_owned())?;
        let cxx = Path::new("/usr/bin/clang++")
            .canonicalize()
            .map_err(|_| "fixed macOS C++ compiler is unavailable".to_owned())?;
        if !linker.is_file() || !cxx.is_file() {
            return Err("fixed macOS linker is not a regular file".into());
        }
        let cmake = [
            "/opt/homebrew/bin/cmake",
            "/usr/local/bin/cmake",
            "/Applications/CMake.app/Contents/bin/cmake",
        ]
        .into_iter()
        .find_map(|candidate| Path::new(candidate).canonicalize().ok())
        .filter(|candidate| candidate.is_file())
        .ok_or_else(|| "CMake is required to verify the installed Rust package".to_owned())?;
        let sdk_output = std::process::Command::new("/usr/bin/xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .env_clear()
            .output()
            .map_err(|_| "fixed macOS SDK lookup failed".to_owned())?;
        let sdk = std::str::from_utf8(&sdk_output.stdout)
            .ok()
            .map(str::trim)
            .filter(|value| sdk_output.status.success() && !value.is_empty())
            .map(Path::new)
            .and_then(|value| value.canonicalize().ok())
            .filter(|value| value.is_dir())
            .ok_or_else(|| "fixed macOS SDK is unavailable".to_owned())?;
        let compiler_flags = format!("-isysroot {} -mmacosx-version-min=14.0", sdk.display());
        environment.insert(
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".into(),
            linker.display().to_string(),
        );
        environment.extend(BTreeMap::from([
            ("BINDGEN_EXTRA_CLANG_ARGS".into(), compiler_flags.clone()),
            ("CC".into(), linker.display().to_string()),
            ("CFLAGS".into(), compiler_flags.clone()),
            ("CMAKE".into(), cmake.display().to_string()),
            ("CXX".into(), cxx.display().to_string()),
            ("CXXFLAGS".into(), compiler_flags),
            ("MACOSX_DEPLOYMENT_TARGET".into(), "14.0".into()),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("SDKROOT".into(), sdk.display().to_string()),
        ]));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_linux_linker(
    platform_target: &str,
    environment: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let linker_variable = match (std::env::consts::ARCH, platform_target) {
        ("x86_64", "x86_64-unknown-linux-gnu") => "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
        ("aarch64", "aarch64-unknown-linux-gnu") => "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        _ => {
            return Err(
                "installed Rust consumer has no fixed Linux linker for this architecture".into()
            );
        }
    };
    let compiler = fixed_linux_file("/usr/bin/gcc", "Linux C compiler")?;
    let cxx = fixed_linux_file("/usr/bin/g++", "Linux C++ compiler")?;
    let librarian = fixed_linux_file("/usr/bin/ar", "Linux librarian")?;
    environment.extend(BTreeMap::from([
        (linker_variable.into(), compiler.clone()),
        ("AR".into(), librarian),
        ("CC".into(), compiler),
        ("CXX".into(), cxx),
    ]));
    Ok(())
}

#[cfg(target_os = "linux")]
fn fixed_linux_file(path: &str, label: &str) -> Result<String, String> {
    let canonical =
        Path::new(path).canonicalize().map_err(|_| format!("fixed {label} is unavailable"))?;
    if !canonical.is_file() {
        return Err(format!("fixed {label} is not a regular file"));
    }
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("fixed {label} path is not Unicode"))
}

#[cfg(not(target_os = "linux"))]
fn configure_linux_linker(_: &str, _: &mut BTreeMap<String, String>) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn configure_windows_linker(
    platform_target: &str,
    environment: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    const MSVC_VERSION: &str = "14.44.35207";
    const SDK_VERSION: &str = "10.0.26100.0";
    if platform_target != "x86_64-pc-windows-msvc" {
        return Ok(());
    }
    if std::env::consts::ARCH != "x86_64" {
        return Err(
            "installed Rust consumer has no fixed Windows linker for this architecture".into()
        );
    }
    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .ok_or_else(|| "fixed Windows system root is unavailable".to_owned())?;
    let system_root = fixed_directory(&system_root, "Windows system root")?;
    let configured_tools = std::env::var_os("VCToolsInstallDir")
        .map(PathBuf::from)
        .ok_or_else(|| "fixed MSVC installation is unavailable".to_owned())?;
    if configured_tools.file_name().and_then(|value| value.to_str()) != Some(MSVC_VERSION) {
        return Err("activated MSVC installation differs from the fixed release toolset".into());
    }
    let tools = fixed_directory(&configured_tools, "MSVC tools")?;
    let tool_bin = fixed_directory(&tools.join("bin/HostX64/x64"), "MSVC executable directory")?;
    let linker = fixed_file(&tool_bin.join("link.exe"), "MSVC linker")?;
    let compiler = fixed_file(&tool_bin.join("cl.exe"), "MSVC compiler")?;
    let librarian = fixed_file(&tool_bin.join("lib.exe"), "MSVC librarian")?;
    let vc_library = fixed_directory(&tools.join("lib/x64"), "MSVC library directory")?;
    let vc_include = fixed_directory(&tools.join("include"), "MSVC include directory")?;

    let configured_sdk = std::env::var_os("WindowsSdkDir")
        .map(PathBuf::from)
        .ok_or_else(|| "fixed Windows SDK is unavailable".to_owned())?;
    let configured_sdk_version = std::env::var("WindowsSDKVersion")
        .map_err(|_| "fixed Windows SDK version is unavailable".to_owned())?;
    if configured_sdk_version.trim_end_matches(['\\', '/']) != SDK_VERSION {
        return Err("activated Windows SDK differs from the fixed release SDK".into());
    }
    let kits = fixed_directory(&configured_sdk, "Windows SDK")?;
    let sdk_bin =
        fixed_directory(&kits.join(format!("bin/{SDK_VERSION}/x64")), "Windows SDK tools")?;
    fixed_file(&sdk_bin.join("rc.exe"), "Windows resource compiler")?;
    let sdk_library = ["ucrt", "um"]
        .map(|kind| {
            fixed_directory(
                &kits.join(format!("Lib/{SDK_VERSION}/{kind}/x64")),
                "Windows SDK library",
            )
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let sdk_include = ["ucrt", "shared", "um", "winrt", "cppwinrt"]
        .map(|kind| {
            fixed_directory(
                &kits.join(format!("Include/{SDK_VERSION}/{kind}")),
                "Windows SDK include",
            )
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

    environment
        .insert("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".into(), linker.display().to_string());
    environment
        .insert("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_AR".into(), librarian.display().to_string());
    environment.insert("CC".into(), compiler.display().to_string());
    environment.insert("CXX".into(), compiler.display().to_string());
    environment.insert("AR".into(), librarian.display().to_string());
    environment.insert(
        "LIB".into(),
        std::iter::once(vc_library)
            .chain(sdk_library)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(";"),
    );
    environment.insert(
        "INCLUDE".into(),
        std::iter::once(vc_include)
            .chain(sdk_include)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(";"),
    );
    environment.insert(
        "PATH".into(),
        [tool_bin, sdk_bin, system_root.join("System32")]
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(";"),
    );
    environment.insert("VCToolsInstallDir".into(), tools.display().to_string());
    environment.insert("WindowsSdkDir".into(), kits.display().to_string());
    environment.insert("WindowsSDKVersion".into(), SDK_VERSION.into());
    Ok(())
}

#[cfg(windows)]
fn fixed_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| format!("fixed {label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(format!("fixed {label} is not a trusted directory"));
    }
    let canonical = path.canonicalize().map_err(|_| format!("fixed {label} is unavailable"))?;
    legacy_windows_path(&canonical, label)
}

#[cfg(windows)]
fn fixed_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| format!("fixed {label} is unavailable"))?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(format!("fixed {label} is not a trusted file"));
    }
    let canonical = path.canonicalize().map_err(|_| format!("fixed {label} is unavailable"))?;
    legacy_windows_path(&canonical, label)
}

#[cfg(windows)]
fn legacy_windows_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let value = path.to_str().ok_or_else(|| format!("fixed {label} path is not Unicode"))?;
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return Ok(PathBuf::from(format!(r"\\{rest}")));
    }
    Ok(PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(value)))
}

#[cfg(not(windows))]
fn configure_windows_linker(_: &str, _: &mut BTreeMap<String, String>) -> Result<(), String> {
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
        cargo_path(&request.rust_library.join("Cargo.toml"))?,
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
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        let tail = diagnostics.char_indices().rev().nth(4_096).map_or(0, |(index, _)| index);
        return Err(format!(
            "installed Rust package metadata failed offline (exit {:?}): {}",
            output.exit_code,
            diagnostics[tail..].trim()
        ));
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
        .map(str::to_owned)
        .ok_or_else(|| "installed Rust package path is not Unicode".to_owned())
}

fn cargo_path(path: &Path) -> Result<String, String> {
    #[cfg(windows)]
    let path = legacy_windows_path(path, "Cargo input")?;
    toml_path(&path)
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
    let fixture_root = std::path::PathBuf::from(
        std::env::args_os().nth(1).expect("installed legacy fixture root"),
    );
    for (name, family) in [("normal.doc", "doc"), ("normal.ppt", "ppt"), ("normal.xls", "xls")] {
        let bytes = std::fs::read(fixture_root.join(name)).expect("installed legacy fixture");
        let request = ConversionRequest::new(InputRef::bytes(bytes, Some(name)));
        let result = block_on(engine.convert(request)).expect("native legacy Office conversion");
        assert!(!result.markdown.is_empty(), "{name} produced empty Markdown");
        assert_eq!(
            result.document.metadata.properties.get("legacyOffice.family").map(String::as_str),
            Some(family),
        );
    }
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

    #[test]
    fn installed_library_snapshot_copies_an_exact_regular_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("snapshot");
        fs::create_dir_all(source.join("vendor/pkg")).unwrap();
        fs::write(source.join("Cargo.toml"), b"manifest\n").unwrap();
        fs::write(source.join("vendor/pkg/file"), b"vendored\n").unwrap();
        snapshot_installed_library(&source, &destination).unwrap();
        assert_eq!(fs::read(destination.join("Cargo.toml")).unwrap(), b"manifest\n");
        assert_eq!(fs::read(destination.join("vendor/pkg/file")).unwrap(), b"vendored\n");
    }

    #[test]
    fn toml_debug_quoting_escapes_windows_path_once() {
        let path = toml_path(Path::new(r"C:\Users\用户\library")).unwrap();
        assert_eq!(format!("{path:?}"), r#""C:\\Users\\用户\\library""#);
    }

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
            audio_fixture: executable.clone(),
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
        validate_installed_metadata(
            &request,
            filesystem_root(temporary.path()).unwrap().as_path(),
            temporary.path(),
            &BTreeMap::new(),
            &executor,
        )
        .unwrap();

        let optional_manifest = library.join("crates/optional/Cargo.toml");
        fs::create_dir_all(optional_manifest.parent().unwrap()).unwrap();
        fs::write(&optional_manifest, b"optional workspace package").unwrap();
        let metadata: CargoMetadata = serde_json::from_value(serde_json::json!({
            "packages": [
                {"id":"root","name":"into-markdown","source":null,"manifest_path":library.join("Cargo.toml")},
                {"id":"optional","name":"optional","source":null,"manifest_path":optional_manifest}
            ],
            "resolve":{"root":"root","nodes":[
                {"id":"root","dependencies":[]},
                {"id":"optional","dependencies":[]}
            ]}
        }))
        .unwrap();
        validate_metadata_authority(&request, &metadata).unwrap();
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
