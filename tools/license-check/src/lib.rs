//! Offline validation for the repository's license policy and inventories.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use toml::Value as TomlValue;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u64,
    allowed: BTreeSet<String>,
    denied: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Component {
    id: String,
    kind: String,
    status: String,
    included_in_release: bool,
    version: Option<String>,
    source: Option<String>,
    license: Option<String>,
    obligations: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Inventory {
    schema_version: u64,
    components: Vec<Component>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtManifest {
    version: String,
    source: String,
    license: String,
    targets: BTreeMap<String, OrtTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtTarget {
    asset: String,
    sha256: String,
}

const SUPPORTED_MODEL_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelManifest {
    schema_version: u64,
    default_bundle: String,
    bundles: Vec<ModelBundle>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelBundle {
    id: String,
    availability: String,
    upstream_version: String,
    languages: Vec<String>,
    platforms: Vec<String>,
    runtime_format: String,
    character_set: ModelCharacterSet,
    runtime_artifacts: Vec<ModelRuntimeArtifact>,
    source_artifacts: Vec<ModelArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCharacterSet {
    status: String,
    source_artifact_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRuntimeArtifact {
    id: String,
    role: String,
    file_name: String,
    url: String,
    sha256: String,
    size: u64,
    platforms: Vec<String>,
    license: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelArtifact {
    id: String,
    role: String,
    url: String,
    sha256: String,
    format: String,
    license: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DownloadManifest {
    schema_version: u64,
    model_files: Vec<ModelDownload>,
    model_runtime_files: Vec<ModelRuntimeDownload>,
    native_archives: Vec<NativeDownload>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ModelDownload {
    artifact_id: String,
    repository: String,
    downloaded_file_path: String,
    url: String,
    sha256: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ModelRuntimeDownload {
    artifact_id: String,
    repository: String,
    downloaded_file_path: String,
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NativeDownload {
    target: String,
    repository: String,
    url: String,
    sha256: String,
    strip_prefix: String,
}

/// Runs the repository audit. Release mode applies distribution-boundary rules.
///
/// # Errors
///
/// Returns every validation error found so maintainers can fix a dependency
/// update in one pass.
pub fn run(release: bool) -> Result<(), Vec<String>> {
    let root = repository_root().map_err(|error| vec![error])?;
    audit(&root, release)
}

/// Runs one immutable CLI mode and rejects all user arguments.
///
/// `release = true` cannot be downgraded by an argument. The dedicated release
/// binary therefore always applies strict release checks.
#[must_use]
pub fn main_for_mode(release: bool) -> ExitCode {
    if !arguments_are_empty(env::args_os().skip(1)) {
        let program = if release { "release-audit" } else { "license-check" };
        eprintln!("usage: {program}");
        return ExitCode::from(2);
    }
    let mode = if release { "release" } else { "check" };
    match run(release) {
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

fn arguments_are_empty(arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> bool {
    arguments.into_iter().next().is_none()
}

fn repository_root() -> Result<PathBuf, String> {
    if let Ok(test_srcdir) = env::var("TEST_SRCDIR") {
        let workspace = env::var("TEST_WORKSPACE").unwrap_or_else(|_| "into_markdown".to_owned());
        let candidate = PathBuf::from(test_srcdir).join(workspace);
        if candidate.join("Cargo.lock").is_file() {
            return Ok(candidate);
        }
    }
    let mut current = env::current_dir().map_err(|error| error.to_string())?;
    loop {
        if current.join("Cargo.lock").is_file() && current.join("MODULE.bazel").is_file() {
            return Ok(current);
        }
        if !current.pop() {
            return Err("cannot locate repository root".to_owned());
        }
    }
}

fn read(path: &Path, errors: &mut Vec<String>) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| {
        errors.push(format!("cannot read {}: {error}", path.display()));
        String::new()
    })
}

fn audit(root: &Path, release: bool) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    validate_project_files(root, &mut errors);

    let policy_text = read(&root.join("third_party/licenses/policy.json"), &mut errors);
    let inventory_text = read(&root.join("third_party/licenses/inventory.json"), &mut errors);
    let approvals_text = read(&root.join("third_party/licenses/rust-lock.tsv"), &mut errors);
    let lock_text = read(&root.join("Cargo.lock"), &mut errors);
    let workspace_packages = validate_workspace_metadata(root, &mut errors);

    let policy: Option<Policy> = parse_json("policy.json", &policy_text, &mut errors);
    let inventory: Option<Inventory> = parse_json("inventory.json", &inventory_text, &mut errors);

    if let Some(policy) = &policy {
        validate_policy(policy, &mut errors);
        validate_rust_lock(&lock_text, &approvals_text, &workspace_packages, policy, &mut errors);
    }
    if let (Some(policy), Some(inventory)) = (&policy, &inventory) {
        validate_inventory(inventory, policy, release, &mut errors);
        validate_existing_manifests(root, inventory, &mut errors);
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    name: &str,
    text: &str,
    errors: &mut Vec<String>,
) -> Option<T> {
    serde_json::from_str(text).map_err(|error| errors.push(format!("invalid {name}: {error}"))).ok()
}

fn validate_project_files(root: &Path, errors: &mut Vec<String>) {
    let license = read(&root.join("LICENSE"), errors);
    if !license.contains("Apache License")
        || !license.contains("Version 2.0, January 2004")
        || !license.contains("END OF TERMS AND CONDITIONS")
    {
        errors.push("LICENSE is not the complete Apache License 2.0 text".to_owned());
    }
    let notice = read(&root.join("NOTICE"), errors);
    if !notice.starts_with("into-markdown\nCopyright 2026 into-markdown contributors") {
        errors.push("NOTICE has an unexpected project attribution".to_owned());
    }
    let notices = read(&root.join("THIRD_PARTY_NOTICES.md"), errors);
    if !notices.contains("third_party/licenses/rust-lock.tsv")
        || !notices.contains("bazel run //tools/license-check:release_audit")
    {
        errors.push(
            "THIRD_PARTY_NOTICES.md does not describe inventory and release audit".to_owned(),
        );
    }
}

fn validate_policy(policy: &Policy, errors: &mut Vec<String>) {
    if policy.schema_version != 1 {
        errors.push("unsupported policy schema_version".to_owned());
    }
    if !policy.allowed.is_disjoint(&policy.denied) {
        errors.push("license allow and deny sets overlap".to_owned());
    }
    if !policy.denied.iter().any(|license| license.starts_with("GPL-")) {
        errors.push("policy must explicitly deny GPL-only conclusions".to_owned());
    }
}

fn validate_license_conclusion(
    owner: &str,
    conclusion: &str,
    policy: &Policy,
    errors: &mut Vec<String>,
) {
    let terms: Vec<_> = conclusion.split(" AND ").collect();
    if terms.is_empty()
        || terms.iter().any(|term| {
            term.is_empty()
                || term.contains(['(', ')'])
                || term.contains(" OR ")
                || term.trim() != *term
        })
    {
        errors.push(format!("{owner} has an invalid concluded SPDX AND expression {conclusion}"));
        return;
    }
    let mut seen = BTreeSet::new();
    for term in terms {
        if !seen.insert(term) {
            errors.push(format!("{owner} repeats concluded license {term}"));
        }
        if !policy.allowed.contains(term) {
            errors.push(format!("{owner} has non-allowed concluded license {term}"));
        }
        if policy.denied.contains(term) {
            errors.push(format!("{owner} has denied concluded license {term}"));
        }
    }
}

fn parse_approvals(text: &str, errors: &mut Vec<String>) -> BTreeMap<(String, String), String> {
    let mut approvals = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
            errors
                .push(format!("rust-lock.tsv:{} must have three tab-separated fields", index + 1));
            continue;
        }
        let key = (fields[0].to_owned(), fields[1].to_owned());
        if approvals.insert(key.clone(), fields[2].to_owned()).is_some() {
            errors.push(format!("duplicate Rust approval {}@{}", key.0, key.1));
        }
    }
    approvals
}

fn validate_rust_lock(
    lock: &str,
    approvals: &str,
    workspace_packages: &BTreeSet<(String, String)>,
    policy: &Policy,
    errors: &mut Vec<String>,
) {
    let approved = parse_approvals(approvals, errors);
    let parsed: TomlValue = match toml::from_str(lock) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock: {error}"));
            return;
        }
    };
    let mut locked = BTreeSet::new();
    for package in parsed.get("package").and_then(TomlValue::as_array).into_iter().flatten() {
        let Some(name) = package.get("name").and_then(TomlValue::as_str) else {
            errors.push("Cargo.lock package lacks name".to_owned());
            continue;
        };
        let Some(version) = package.get("version").and_then(TomlValue::as_str) else {
            errors.push(format!("Cargo.lock package {name} lacks version"));
            continue;
        };
        let key = (name.to_owned(), version.to_owned());
        let source = package.get("source").and_then(TomlValue::as_str);
        if source.is_none() {
            if !workspace_packages.contains(&key) {
                errors.push(format!(
                    "source-less package {name}@{version} is not an exact workspace member"
                ));
            }
            continue;
        }
        if source != Some("registry+https://github.com/rust-lang/crates.io-index") {
            errors
                .push(format!("Rust dependency {name}@{version} has unreviewed source {source:?}"));
        }
        let checksum = package.get("checksum").and_then(TomlValue::as_str);
        if checksum.is_none_or(|value| !is_sha256(value)) {
            errors.push(format!("Rust dependency {name}@{version} lacks a valid SHA-256"));
        }
        locked.insert(key);
    }

    for key in locked.difference(&approved.keys().cloned().collect()) {
        errors.push(format!("unreviewed Rust dependency {}@{}", key.0, key.1));
    }
    for key in approved.keys().collect::<BTreeSet<_>>().difference(&locked.iter().collect()) {
        errors.push(format!("stale Rust approval {}@{}", key.0, key.1));
    }
    for ((name, version), license) in approved {
        validate_license_conclusion(
            &format!("Rust dependency {name}@{version}"),
            &license,
            policy,
            errors,
        );
    }
}

fn validate_inventory(
    inventory: &Inventory,
    policy: &Policy,
    release: bool,
    errors: &mut Vec<String>,
) {
    if inventory.schema_version != 1 {
        errors.push("unsupported inventory schema_version".to_owned());
    }
    let mut ids = BTreeSet::new();
    for component in &inventory.components {
        if !ids.insert(component.id.as_str()) {
            errors.push(format!("duplicate component {}", component.id));
        }
        if component.kind.trim().is_empty() {
            errors.push(format!("component {} has no kind", component.id));
        }
        match component.status.as_str() {
            "reviewed" => {
                for (field, value) in [
                    ("version", &component.version),
                    ("source", &component.source),
                    ("license", &component.license),
                    ("obligations", &component.obligations),
                ] {
                    if value.as_deref().is_none_or(str::is_empty) {
                        errors.push(format!("reviewed component {} lacks {field}", component.id));
                    }
                }
                if let Some(license) = &component.license {
                    validate_license_conclusion(
                        &format!("component {}", component.id),
                        license,
                        policy,
                        errors,
                    );
                }
            }
            "planned" => {}
            other => errors.push(format!("component {} has unknown status {other}", component.id)),
        }
        if release && component.included_in_release && component.status != "reviewed" {
            errors.push(format!("release component {} is not reviewed", component.id));
        }
    }
    for required in [
        "pdfium",
        "ffmpeg",
        "libreoffice",
        "wasmtime",
        "generated-onnx-models",
        "distribution-fonts",
        "onnxruntime-cpu",
        "pp-ocrv6-tiny-detector-source",
        "pp-ocrv6-tiny-recognizer-source",
    ] {
        if !ids.contains(required) {
            errors.push(format!("required inventory component {required} is missing"));
        }
    }
}

fn validate_existing_manifests(root: &Path, inventory: &Inventory, errors: &mut Vec<String>) {
    let ort_text = read(&root.join("third_party/onnxruntime/manifest.json"), errors);
    let models_text = read(&root.join("models/manifest.json"), errors);
    let downloads_text = read(&root.join("third_party/licenses/downloads.json"), errors);
    let ort: Option<OrtManifest> = parse_json("ONNX Runtime manifest", &ort_text, errors);
    let models: Option<ModelManifest> = parse_json("model manifest", &models_text, errors);
    let downloads: Option<DownloadManifest> =
        parse_json("download manifest", &downloads_text, errors);

    if let (Some(ort), Some(downloads)) = (&ort, &downloads) {
        validate_ort_manifest(inventory, ort, downloads, errors);
    }
    if let (Some(models), Some(downloads)) = (&models, &downloads) {
        validate_model_manifest(inventory, models, downloads, errors);
    }
    if let Some(downloads) = &downloads {
        if downloads.schema_version != 1 {
            errors.push("unsupported download manifest schema_version".to_owned());
        }
        validate_download_fields(downloads, errors);
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_safe_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn is_safe_model_file_name(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path.components().count() == 1
        && matches!(path.components().next(), Some(std::path::Component::Normal(_)))
}

fn is_canonical_https(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn validate_download_fields(downloads: &DownloadManifest, errors: &mut Vec<String>) {
    let mut repositories = BTreeSet::new();
    let mut runtime_ids = BTreeSet::new();
    for item in &downloads.model_files {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !is_safe_model_id(&item.artifact_id)
            || !is_safe_model_id(&item.repository)
            || !is_safe_model_file_name(&item.downloaded_file_path)
            || !is_canonical_https(&item.url)
            || !is_sha256(&item.sha256)
        {
            errors.push(format!("model download {} has incomplete fields", item.repository));
        }
    }
    for item in &downloads.model_runtime_files {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !is_safe_model_id(&item.artifact_id)
            || !is_safe_model_id(&item.repository)
            || !is_safe_model_file_name(&item.downloaded_file_path)
            || !is_canonical_https(&item.url)
            || !is_sha256(&item.sha256)
            || item.size == 0
        {
            errors
                .push(format!("model runtime download {} has incomplete fields", item.repository));
        }
        if !runtime_ids.insert(item.artifact_id.as_str()) {
            errors.push(format!("duplicate runtime download artifact {}", item.artifact_id));
        }
    }
    for item in &downloads.native_archives {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !SUPPORTED_MODEL_TARGETS.contains(&item.target.as_str())
            || !is_safe_model_id(&item.repository)
            || !is_safe_model_file_name(&item.strip_prefix)
            || !is_canonical_https(&item.url)
            || !is_sha256(&item.sha256)
        {
            errors.push(format!("native download {} has incomplete fields", item.repository));
        }
    }
}

fn exact_component<'a>(
    inventory: &'a Inventory,
    id: &str,
    errors: &mut Vec<String>,
) -> Option<&'a Component> {
    let matches: Vec<_> = inventory.components.iter().filter(|item| item.id == id).collect();
    match matches.as_slice() {
        [component] => Some(*component),
        [] => {
            errors.push(format!("required inventory component {id} is missing"));
            None
        }
        _ => {
            errors.push(format!("required inventory component {id} is duplicated"));
            None
        }
    }
}

fn validate_ort_manifest(
    inventory: &Inventory,
    manifest: &OrtManifest,
    downloads: &DownloadManifest,
    errors: &mut Vec<String>,
) {
    let Some(component) = exact_component(inventory, "onnxruntime-cpu", errors) else {
        return;
    };
    if component.kind != "native-runtime" {
        errors.push("component onnxruntime-cpu must have kind native-runtime".to_owned());
    }
    if component.status != "reviewed" {
        errors.push("component onnxruntime-cpu must be reviewed".to_owned());
    }
    for (field, actual, expected) in [
        ("version", component.version.as_deref(), Some(manifest.version.as_str())),
        ("source", component.source.as_deref(), Some(manifest.source.as_str())),
        ("license", component.license.as_deref(), Some(manifest.license.as_str())),
    ] {
        if actual != expected {
            errors.push(format!("component onnxruntime-cpu {field} disagrees with its manifest"));
        }
    }
    let expected_source =
        format!("https://github.com/microsoft/onnxruntime/releases/tag/v{}", manifest.version);
    if manifest.version.is_empty()
        || manifest.source != expected_source
        || !is_canonical_https(&manifest.source)
        || manifest.license != "MIT"
    {
        errors.push("ONNX Runtime source is not the versioned upstream release tag".to_owned());
    }

    let expected_targets = BTreeMap::from([
        ("aarch64-apple-darwin", "onnxruntime_macos_arm64"),
        ("aarch64-unknown-linux-gnu", "onnxruntime_linux_arm64"),
        ("x86_64-pc-windows-msvc", "onnxruntime_windows_x86_64"),
        ("x86_64-unknown-linux-gnu", "onnxruntime_linux_x86_64"),
    ]);
    let actual_keys: BTreeSet<_> = manifest.targets.keys().map(String::as_str).collect();
    let expected_keys: BTreeSet<_> = expected_targets.keys().copied().collect();
    if actual_keys != expected_keys {
        errors.push(
            "ONNX Runtime manifest must contain the exact four supported target keys".to_owned(),
        );
    }
    let mut by_target = BTreeMap::new();
    for download in &downloads.native_archives {
        if by_target.insert(download.target.as_str(), download).is_some() {
            errors.push(format!("duplicate native download target {}", download.target));
        }
    }
    let download_targets: BTreeSet<_> = by_target.keys().copied().collect();
    if download_targets != expected_keys {
        errors.push(
            "download manifest native targets do not match supported ONNX targets".to_owned(),
        );
    }
    for (target, repository) in expected_targets {
        let (Some(asset), Some(download)) =
            (manifest.targets.get(target), by_target.get(target).copied())
        else {
            continue;
        };
        if !is_safe_model_file_name(&asset.asset) || !is_sha256(&asset.sha256) {
            errors.push(format!("ONNX Runtime target {target} lacks a valid SHA-256"));
        }
        let expected_url = format!(
            "https://github.com/microsoft/onnxruntime/releases/download/v{}/{}",
            manifest.version, asset.asset
        );
        let expected_strip_prefix =
            asset.asset.strip_suffix(".tgz").or_else(|| asset.asset.strip_suffix(".zip"));
        if download.repository != repository
            || download.url != expected_url
            || download.sha256 != asset.sha256
            || Some(download.strip_prefix.as_str()) != expected_strip_prefix
        {
            errors.push(format!(
                "ONNX Runtime target {target} disagrees with authoritative download {repository}"
            ));
        }
    }
}

fn validate_model_manifest(
    inventory: &Inventory,
    manifest: &ModelManifest,
    downloads: &DownloadManifest,
    errors: &mut Vec<String>,
) {
    if manifest.schema_version != 1 {
        errors.push("unsupported model manifest schema_version".to_owned());
    }
    let (artifacts, bundles) = collect_model_artifacts(manifest, downloads, errors);
    validate_default_model_bundle(manifest, &bundles, errors);

    let mut inventory_sources = BTreeMap::new();
    for component in inventory.components.iter().filter(|item| item.kind == "model-source") {
        if inventory_sources.insert(component.id.as_str(), component).is_some() {
            errors.push(format!("duplicate model-source inventory component {}", component.id));
        }
    }
    for id in artifacts.keys() {
        if !inventory_sources.contains_key(id) {
            errors.push(format!("model artifact {id} has no inventory component"));
        }
    }
    for id in inventory_sources.keys() {
        if !artifacts.contains_key(id) {
            errors.push(format!("model-source inventory component {id} has no manifest artifact"));
        }
    }

    let mut downloads_by_artifact = BTreeMap::new();
    for download in &downloads.model_files {
        if downloads_by_artifact.insert(download.artifact_id.as_str(), download).is_some() {
            errors.push(format!("duplicate model download artifact {}", download.artifact_id));
        }
    }
    let artifact_ids: BTreeSet<_> = artifacts.keys().copied().collect();
    let download_ids: BTreeSet<_> = downloads_by_artifact.keys().copied().collect();
    if artifact_ids != download_ids {
        errors.push(
            "authoritative model downloads do not match all model manifest artifacts".to_owned(),
        );
    }

    for (id, (artifact, _bundle_id, upstream_version)) in artifacts {
        validate_model_artifact(
            id,
            artifact,
            upstream_version,
            &inventory_sources,
            &downloads_by_artifact,
            errors,
        );
    }
}

type ModelArtifacts<'a> = BTreeMap<&'a str, (&'a ModelArtifact, &'a str, &'a str)>;

fn collect_model_artifacts<'a>(
    manifest: &'a ModelManifest,
    downloads: &'a DownloadManifest,
    errors: &mut Vec<String>,
) -> (ModelArtifacts<'a>, BTreeMap<&'a str, &'a ModelBundle>) {
    let runtime_downloads: BTreeMap<_, _> = downloads
        .model_runtime_files
        .iter()
        .map(|item| (item.artifact_id.as_str(), item))
        .collect();
    let mut artifacts = BTreeMap::new();
    let mut bundles = BTreeMap::new();
    let mut runtime_ids = BTreeSet::new();
    for bundle in &manifest.bundles {
        validate_runtime_bundle(bundle, &runtime_downloads, &mut runtime_ids, errors);
        if bundle.id.is_empty() {
            errors.push("model bundle ID must not be empty".to_owned());
        }
        if bundles.insert(bundle.id.as_str(), bundle).is_some() {
            errors.push(format!("duplicate model bundle ID {}", bundle.id));
        }
        let mut roles = BTreeMap::new();
        for artifact in &bundle.source_artifacts {
            if roles.insert(artifact.role.as_str(), artifact.id.as_str()).is_some() {
                errors.push(format!(
                    "model bundle {} has duplicate role {}",
                    bundle.id, artifact.role
                ));
            }
            if artifacts
                .insert(
                    artifact.id.as_str(),
                    (artifact, bundle.id.as_str(), bundle.upstream_version.as_str()),
                )
                .is_some()
            {
                errors.push(format!("duplicate model artifact {} across bundles", artifact.id));
            }
        }
        let required_roles = BTreeSet::from(["detector", "recognizer-and-dictionary"]);
        if roles.keys().copied().collect::<BTreeSet<_>>() != required_roles {
            errors.push(format!(
                "OCR model bundle {} must have exactly the required source roles",
                bundle.id
            ));
        }
        for required_role in required_roles {
            if !roles.contains_key(required_role) {
                errors.push(format!(
                    "OCR model bundle {} lacks required role {required_role}",
                    bundle.id
                ));
            }
        }
    }
    if runtime_ids != runtime_downloads.keys().copied().collect() {
        errors.push(
            "authoritative runtime model downloads do not match all runtime artifacts".to_owned(),
        );
    }
    (artifacts, bundles)
}

fn validate_runtime_bundle<'a>(
    bundle: &'a ModelBundle,
    runtime_downloads: &BTreeMap<&str, &ModelRuntimeDownload>,
    runtime_ids: &mut BTreeSet<&'a str>,
    errors: &mut Vec<String>,
) {
    if !is_safe_model_id(&bundle.id) {
        errors.push(format!("model bundle {:?} has unsafe ID", bundle.id));
    }
    if !matches!(bundle.availability.as_str(), "planned" | "available") {
        errors.push(format!("model bundle {} has invalid availability", bundle.id));
    }
    let expected_targets = BTreeSet::from(SUPPORTED_MODEL_TARGETS);
    let bundle_targets = bundle.platforms.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if bundle.platforms.len() != SUPPORTED_MODEL_TARGETS.len() || bundle_targets != expected_targets
    {
        errors.push(format!("model bundle {} must declare exact supported targets", bundle.id));
    }
    if bundle.languages.is_empty()
        || bundle.runtime_format.is_empty()
        || bundle.upstream_version.is_empty()
    {
        errors.push(format!("model bundle {} has incomplete metadata", bundle.id));
    }
    if !matches!(bundle.character_set.status.as_str(), "planned" | "available")
        || !bundle.source_artifacts.iter().any(|item| {
            item.id == bundle.character_set.source_artifact_id
                && item.role == "recognizer-and-dictionary"
        })
    {
        errors.push(format!("model bundle {} has invalid character-set provenance", bundle.id));
    }
    let installable = bundle.availability == "available"
        && bundle.character_set.status == "available"
        && !bundle.runtime_artifacts.is_empty();
    if (bundle.availability == "available") != installable {
        errors.push(format!(
            "model bundle {} is available without complete runtime files",
            bundle.id
        ));
    }
    let mut runtime_roles = BTreeSet::new();
    for artifact in &bundle.runtime_artifacts {
        if !runtime_ids.insert(artifact.id.as_str()) {
            errors.push(format!("duplicate runtime model artifact {}", artifact.id));
        }
        let platforms = artifact.platforms.iter().map(String::as_str).collect::<BTreeSet<_>>();
        if !runtime_roles.insert(artifact.role.as_str()) {
            errors.push(format!(
                "model bundle {} has duplicate runtime role {}",
                bundle.id, artifact.role
            ));
        }
        if !is_safe_model_id(&artifact.id)
            || !is_safe_model_file_name(&artifact.file_name)
            || !is_canonical_https(&artifact.url)
            || !is_sha256(&artifact.sha256)
            || artifact.size == 0
            || artifact.role.is_empty()
            || platforms.is_empty()
            || platforms.len() != artifact.platforms.len()
            || !platforms.iter().all(|target| SUPPORTED_MODEL_TARGETS.contains(target))
            || artifact.license.is_empty()
        {
            errors.push(format!("runtime model artifact {} is incomplete", artifact.id));
        }
        match runtime_downloads.get(artifact.id.as_str()) {
            Some(download)
                if download.downloaded_file_path == artifact.file_name
                    && download.url == artifact.url
                    && download.sha256 == artifact.sha256
                    && download.size == artifact.size => {}
            _ => errors.push(format!(
                "runtime model artifact {} disagrees with authoritative download",
                artifact.id
            )),
        }
    }
    if !bundle.runtime_artifacts.is_empty()
        && runtime_roles != BTreeSet::from(["detector", "recognizer-and-dictionary"])
    {
        errors.push(format!(
            "model bundle {} must have exactly the required runtime roles",
            bundle.id
        ));
    }
}

fn validate_default_model_bundle(
    manifest: &ModelManifest,
    bundles: &BTreeMap<&str, &ModelBundle>,
    errors: &mut Vec<String>,
) {
    if manifest.default_bundle.is_empty() {
        errors.push("model manifest default_bundle must not be empty".to_owned());
    }
    let default = bundles.get(manifest.default_bundle.as_str()).copied();
    if default.is_none() {
        errors
            .push(format!("model manifest default bundle {} is missing", manifest.default_bundle));
    }
    for (required_id, required_role) in [
        ("pp-ocrv6-tiny-detector-source", "detector"),
        ("pp-ocrv6-tiny-recognizer-source", "recognizer-and-dictionary"),
    ] {
        if default.is_none_or(|bundle| {
            !bundle
                .source_artifacts
                .iter()
                .any(|artifact| artifact.id == required_id && artifact.role == required_role)
        }) {
            errors.push(format!(
                "default OCR bundle {} lacks required artifact {required_id} with role {required_role}",
                manifest.default_bundle
            ));
        }
    }
}

fn validate_model_artifact(
    id: &str,
    artifact: &ModelArtifact,
    upstream_version: &str,
    inventory_sources: &BTreeMap<&str, &Component>,
    downloads_by_artifact: &BTreeMap<&str, &ModelDownload>,
    errors: &mut Vec<String>,
) {
    if !is_safe_model_id(id)
        || artifact.role.is_empty()
        || artifact.format.is_empty()
        || artifact.license.is_empty()
        || !is_canonical_https(&artifact.url)
    {
        errors.push(format!("model manifest entry {id} has incomplete fields"));
    }
    if let Some(component) = inventory_sources.get(id) {
        if component.status != "reviewed" {
            errors.push(format!("managed model component {id} must be reviewed"));
        }
        for (field, actual, expected) in [
            ("version", component.version.as_deref(), Some(upstream_version)),
            ("source", component.source.as_deref(), Some(artifact.url.as_str())),
            ("license", component.license.as_deref(), Some(artifact.license.as_str())),
        ] {
            if actual != expected {
                errors.push(format!("model component {id} {field} disagrees with its manifest"));
            }
        }
    }
    if !is_sha256(&artifact.sha256) {
        errors.push(format!("model manifest entry {id} lacks a valid SHA-256"));
    }
    match downloads_by_artifact.get(id).copied() {
        Some(download) if download.url == artifact.url && download.sha256 == artifact.sha256 => {}
        Some(_) => errors
            .push(format!("model artifact {id} URL/hash disagrees with authoritative download")),
        None => errors.push(format!("model artifact {id} lacks authoritative download")),
    }
}

fn validate_workspace_metadata(
    root: &Path,
    errors: &mut Vec<String>,
) -> BTreeSet<(String, String)> {
    let mut packages = BTreeSet::new();
    let root_text = read(&root.join("Cargo.toml"), errors);
    let manifest: TomlValue = match toml::from_str(&root_text) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid workspace Cargo.toml: {error}"));
            return packages;
        }
    };
    let package = manifest.get("workspace").and_then(|value| value.get("package"));
    if package.and_then(|value| value.get("publish")).and_then(TomlValue::as_bool) != Some(false) {
        errors.push("workspace crates must remain publish = false".to_owned());
    }
    if package.and_then(|value| value.get("license")).and_then(TomlValue::as_str)
        != Some("Apache-2.0")
    {
        errors.push("workspace.package.license must be Apache-2.0".to_owned());
    }
    let members = manifest
        .get("workspace")
        .and_then(|value| value.get("members"))
        .and_then(TomlValue::as_array)
        .into_iter()
        .flatten();
    for member in members.filter_map(TomlValue::as_str) {
        let text = read(&root.join(member).join("Cargo.toml"), errors);
        match toml::from_str::<TomlValue>(&text) {
            Ok(value) => {
                let Some(member_package) = value.get("package") else {
                    errors.push(format!("{member}/Cargo.toml has no package table"));
                    continue;
                };
                if member_package
                    .get("license")
                    .and_then(|license| license.get("workspace"))
                    .and_then(TomlValue::as_bool)
                    != Some(true)
                {
                    errors.push(format!("{member}/Cargo.toml must inherit workspace license"));
                }
                let Some(name) = member_package.get("name").and_then(TomlValue::as_str) else {
                    errors.push(format!("{member}/Cargo.toml package lacks name"));
                    continue;
                };
                let version =
                    member_package.get("version").and_then(TomlValue::as_str).or_else(|| {
                        member_package
                            .get("version")
                            .and_then(|value| value.get("workspace"))
                            .and_then(TomlValue::as_bool)
                            .filter(|enabled| *enabled)
                            .and_then(|_| {
                                package
                                    .and_then(|value| value.get("version"))
                                    .and_then(TomlValue::as_str)
                            })
                    });
                let Some(version) = version else {
                    errors.push(format!("{member}/Cargo.toml package lacks resolvable version"));
                    continue;
                };
                if !packages.insert((name.to_owned(), version.to_owned())) {
                    errors.push(format!("duplicate workspace package {name}@{version}"));
                }
            }
            Err(error) => errors.push(format!("invalid {member}/Cargo.toml: {error}")),
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            schema_version: 1,
            allowed: BTreeSet::from([
                "Apache-2.0".to_owned(),
                "MIT".to_owned(),
                "Unicode-3.0".to_owned(),
            ]),
            denied: BTreeSet::from(["GPL-3.0-only".to_owned()]),
        }
    }

    fn planned_component(id: &str) -> Component {
        Component {
            id: id.to_owned(),
            kind: "placeholder".to_owned(),
            status: "planned".to_owned(),
            included_in_release: false,
            version: None,
            source: None,
            license: None,
            obligations: None,
        }
    }

    fn model_component(id: &str, artifact: &ModelArtifact, version: &str) -> Component {
        Component {
            id: id.to_owned(),
            kind: "model-source".to_owned(),
            status: "reviewed".to_owned(),
            included_in_release: false,
            version: Some(version.to_owned()),
            source: Some(artifact.url.clone()),
            license: Some(artifact.license.clone()),
            obligations: Some("preserve upstream terms".to_owned()),
        }
    }

    fn artifact(id: &str, role: &str, marker: char) -> ModelArtifact {
        ModelArtifact {
            id: id.to_owned(),
            role: role.to_owned(),
            url: format!("https://example.invalid/{id}.tar"),
            sha256: marker.to_string().repeat(64),
            format: "tar".to_owned(),
            license: "Apache-2.0".to_owned(),
        }
    }

    fn empty_downloads() -> DownloadManifest {
        DownloadManifest {
            schema_version: 1,
            model_files: vec![],
            model_runtime_files: vec![],
            native_archives: vec![],
        }
    }

    fn bundle(id: &str, version: &str, source_artifacts: Vec<ModelArtifact>) -> ModelBundle {
        ModelBundle {
            id: id.to_owned(),
            availability: "planned".to_owned(),
            upstream_version: version.to_owned(),
            languages: vec!["zh".to_owned(), "en".to_owned()],
            platforms: vec![
                "aarch64-apple-darwin".to_owned(),
                "x86_64-unknown-linux-gnu".to_owned(),
                "aarch64-unknown-linux-gnu".to_owned(),
                "x86_64-pc-windows-msvc".to_owned(),
            ],
            runtime_format: "onnx".to_owned(),
            character_set: ModelCharacterSet {
                status: "planned".to_owned(),
                source_artifact_id: source_artifacts
                    .last()
                    .map_or_else(|| "absent".to_owned(), |artifact| artifact.id.clone()),
            },
            runtime_artifacts: vec![],
            source_artifacts,
        }
    }

    fn model_fixture() -> (ModelManifest, Inventory, DownloadManifest) {
        let detector = artifact("pp-ocrv6-tiny-detector-source", "detector", 'a');
        let recognizer =
            artifact("pp-ocrv6-tiny-recognizer-source", "recognizer-and-dictionary", 'b');
        let version = "test model";
        let inventory = Inventory {
            schema_version: 1,
            components: vec![
                model_component(&detector.id, &detector, version),
                model_component(&recognizer.id, &recognizer, version),
            ],
        };
        let downloads = DownloadManifest {
            schema_version: 1,
            model_files: vec![
                ModelDownload {
                    artifact_id: detector.id.clone(),
                    repository: "ppocrv6_tiny_detector_source".to_owned(),
                    downloaded_file_path: "detector.tar".to_owned(),
                    url: detector.url.clone(),
                    sha256: detector.sha256.clone(),
                },
                ModelDownload {
                    artifact_id: recognizer.id.clone(),
                    repository: "ppocrv6_tiny_recognizer_source".to_owned(),
                    downloaded_file_path: "recognizer.tar".to_owned(),
                    url: recognizer.url.clone(),
                    sha256: recognizer.sha256.clone(),
                },
            ],
            model_runtime_files: vec![],
            native_archives: vec![],
        };
        let manifest = ModelManifest {
            schema_version: 1,
            default_bundle: "default".to_owned(),
            bundles: vec![bundle("default", version, vec![detector, recognizer])],
        };
        (manifest, inventory, downloads)
    }

    fn runtime_fixture() -> (ModelRuntimeArtifact, ModelRuntimeDownload) {
        let artifact = ModelRuntimeArtifact {
            id: "reviewed-runtime".to_owned(),
            role: "detector".to_owned(),
            file_name: "model.onnx".to_owned(),
            url: "https://example.invalid/model.onnx".to_owned(),
            sha256: "a".repeat(64),
            size: 5,
            platforms: vec!["aarch64-apple-darwin".to_owned()],
            license: "Apache-2.0".to_owned(),
        };
        let download = ModelRuntimeDownload {
            artifact_id: artifact.id.clone(),
            repository: "reviewed_runtime".to_owned(),
            downloaded_file_path: artifact.file_name.clone(),
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
            size: artifact.size,
        };
        (artifact, download)
    }

    fn ort_fixture() -> (OrtManifest, Inventory, DownloadManifest) {
        let version = "1.2.3";
        let target_repositories = [
            ("aarch64-apple-darwin", "onnxruntime_macos_arm64", "mac.tgz", 'a'),
            ("aarch64-unknown-linux-gnu", "onnxruntime_linux_arm64", "arm.tgz", 'b'),
            ("x86_64-pc-windows-msvc", "onnxruntime_windows_x86_64", "windows.zip", 'c'),
            ("x86_64-unknown-linux-gnu", "onnxruntime_linux_x86_64", "linux.tgz", 'd'),
        ];
        let mut targets = BTreeMap::new();
        let mut native_archives = Vec::new();
        for (target, repository, asset, marker) in target_repositories {
            let sha256 = marker.to_string().repeat(64);
            targets.insert(
                target.to_owned(),
                OrtTarget { asset: asset.to_owned(), sha256: sha256.clone() },
            );
            native_archives.push(NativeDownload {
                target: target.to_owned(),
                repository: repository.to_owned(),
                url: format!(
                    "https://github.com/microsoft/onnxruntime/releases/download/v{version}/{asset}"
                ),
                sha256,
                strip_prefix: asset
                    .strip_suffix(".tgz")
                    .or_else(|| asset.strip_suffix(".zip"))
                    .unwrap()
                    .to_owned(),
            });
        }
        let source = format!("https://github.com/microsoft/onnxruntime/releases/tag/v{version}");
        let manifest = OrtManifest {
            version: version.to_owned(),
            source: source.clone(),
            license: "MIT".to_owned(),
            targets,
        };
        let inventory = Inventory {
            schema_version: 1,
            components: vec![Component {
                id: "onnxruntime-cpu".to_owned(),
                kind: "native-runtime".to_owned(),
                status: "reviewed".to_owned(),
                included_in_release: false,
                version: Some(version.to_owned()),
                source: Some(source),
                license: Some("MIT".to_owned()),
                obligations: Some("preserve MIT".to_owned()),
            }],
        };
        (
            manifest,
            inventory,
            DownloadManifest {
                schema_version: 1,
                model_files: vec![],
                model_runtime_files: vec![],
                native_archives,
            },
        )
    }

    #[test]
    fn release_strictness_cannot_be_disabled() {
        let mut components = [
            "pdfium",
            "ffmpeg",
            "libreoffice",
            "wasmtime",
            "generated-onnx-models",
            "distribution-fonts",
        ]
        .map(planned_component);
        components[1].included_in_release = true;
        let inventory = Inventory { schema_version: 1, components: components.into() };
        let mut errors = Vec::new();
        validate_inventory(&inventory, &policy(), true, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains("ffmpeg") && error.contains("not reviewed"))
        );

        let mut parse_errors = Vec::new();
        let parsed: Option<Policy> = parse_json(
            "policy",
            r#"{"schema_version":1,"allowed":["MIT"],"denied":[],"require_known_for_release":false}"#,
            &mut parse_errors,
        );
        assert!(parsed.is_none());
        assert!(parse_errors.iter().any(|error| error.contains("unknown field")));
    }

    #[test]
    fn approvals_reject_duplicates() {
        let mut errors = Vec::new();
        parse_approvals("same\t1.0.0\tMIT\nsame\t1.0.0\tMIT\n", &mut errors);
        assert_eq!(errors, ["duplicate Rust approval same@1.0.0"]);
    }

    #[test]
    fn lock_rejects_unreviewed_source_and_stale_approval() {
        let lock = r#"version = 4
[[package]]
name = "new"
version = "1.0.0"
source = "git+https://example.invalid/new"
checksum = "not-a-hash"
"#;
        let mut errors = Vec::new();
        validate_rust_lock(lock, "old\t1.0.0\tMIT\n", &BTreeSet::new(), &policy(), &mut errors);
        assert!(errors.iter().any(|error| error.contains("unreviewed source")));
        assert!(errors.iter().any(|error| error.contains("unreviewed Rust dependency new@1.0.0")));
        assert!(errors.iter().any(|error| error.contains("stale Rust approval old@1.0.0")));
    }

    #[test]
    fn lock_rejects_source_less_non_workspace_package() {
        let lock = r#"version = 4
[[package]]
name = "workspace"
version = "0.0.0"
[[package]]
name = "malicious-path-dependency"
version = "9.9.9"
"#;
        let workspace = BTreeSet::from([("workspace".to_owned(), "0.0.0".to_owned())]);
        let mut errors = Vec::new();
        validate_rust_lock(lock, "", &workspace, &policy(), &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains("malicious-path-dependency@9.9.9")
                && error.contains("not an exact workspace member")
        }));
    }

    #[test]
    fn conjunctive_license_conclusions_validate_every_term() {
        let mut errors = Vec::new();
        validate_license_conclusion("unicode-ident", "MIT AND Unicode-3.0", &policy(), &mut errors);
        assert!(errors.is_empty());

        validate_license_conclusion("bad", "MIT AND GPL-3.0-only", &policy(), &mut errors);
        assert!(errors.iter().any(|error| error.contains("denied concluded license")));
    }

    #[test]
    fn fixed_cli_modes_reject_override_arguments() {
        assert!(arguments_are_empty(Vec::<&str>::new()));
        assert!(!arguments_are_empty(["check"]));
        assert!(!arguments_are_empty(["release"]));
    }

    #[test]
    fn missing_onnx_inventory_component_is_rejected() {
        let manifest = OrtManifest {
            version: "1.0.0".to_owned(),
            source: "https://github.com/microsoft/onnxruntime/releases/tag/v1.0.0".to_owned(),
            license: "MIT".to_owned(),
            targets: BTreeMap::new(),
        };
        let mut errors = Vec::new();
        validate_ort_manifest(
            &Inventory { schema_version: 1, components: vec![] },
            &manifest,
            &empty_downloads(),
            &mut errors,
        );
        assert!(errors.iter().any(|error| error.contains("onnxruntime-cpu is missing")));
    }

    #[test]
    fn onnx_authoritative_download_platform_url_and_hash_drift_is_rejected() {
        let (mut manifest, inventory, mut downloads) = ort_fixture();
        downloads
            .native_archives
            .iter_mut()
            .find(|download| download.target == "aarch64-apple-darwin")
            .unwrap()
            .url
            .push_str(".wrong");
        manifest.targets.remove("x86_64-pc-windows-msvc");
        let mut errors = Vec::new();
        validate_ort_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("exact four supported target keys")));
        assert!(errors.iter().any(|error| {
            error.contains("aarch64-apple-darwin")
                && error.contains("disagrees with authoritative download")
        }));

        for mutate in ["repository", "sha256", "strip_prefix"] {
            let (manifest, inventory, mut downloads) = ort_fixture();
            let download = downloads
                .native_archives
                .iter_mut()
                .find(|download| download.target == "aarch64-apple-darwin")
                .unwrap();
            match mutate {
                "repository" => download.repository = "wrong_repository".into(),
                "sha256" => download.sha256 = "f".repeat(64),
                "strip_prefix" => download.strip_prefix = "wrong-prefix".into(),
                _ => unreachable!(),
            }
            let mut errors = Vec::new();
            validate_ort_manifest(&inventory, &manifest, &downloads, &mut errors);
            assert!(
                errors.iter().any(|error| {
                    error.contains("aarch64-apple-darwin")
                        && error.contains("disagrees with authoritative download")
                }),
                "mutation {mutate}"
            );
        }
    }

    #[test]
    fn deleted_model_inventory_entry_is_rejected() {
        let (manifest, mut inventory, downloads) = model_fixture();
        inventory.components.pop();
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("recognizer-source has no inventory")));
    }

    #[test]
    fn empty_default_bundle_is_rejected() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.default_bundle.clear();
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("default_bundle must not be empty")));
    }

    #[test]
    fn missing_default_bundle_is_rejected() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.default_bundle = "absent".to_owned();
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("default bundle absent is missing")));
    }

    #[test]
    fn duplicate_bundle_id_is_rejected() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(bundle("default", "duplicate", vec![]));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("duplicate model bundle ID default")));
    }

    #[test]
    fn default_bundle_must_contain_required_roles_and_artifacts_itself() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles[0].source_artifacts.pop();
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains("default OCR bundle default lacks required artifact")
                && error.contains("recognizer-and-dictionary")
        }));
    }

    #[test]
    fn every_ocr_bundle_requires_complete_roles() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(bundle(
            "incomplete",
            "incomplete",
            vec![artifact("only-detector", "detector", 'f')],
        ));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains(
                "OCR model bundle incomplete lacks required role recognizer-and-dictionary",
            )
        }));
    }

    #[test]
    fn artifact_in_later_bundle_is_not_ignored() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(bundle(
            "later",
            "later",
            vec![artifact("later-bundle-artifact", "detector", 'c')],
        ));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains("later-bundle-artifact has no inventory"))
        );
    }

    #[test]
    fn duplicate_model_artifact_across_bundles_is_rejected() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(bundle(
            "duplicate-artifact",
            "duplicate",
            vec![artifact("pp-ocrv6-tiny-detector-source", "detector", 'd')],
        ));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("duplicate model artifact")));
    }

    #[test]
    fn orphan_model_inventory_component_is_rejected() {
        let (manifest, mut inventory, downloads) = model_fixture();
        let orphan = artifact("orphan", "detector", 'e');
        inventory.components.push(model_component(&orphan.id, &orphan, "orphan"));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("orphan has no manifest artifact")));
    }

    #[test]
    fn model_authoritative_download_url_and_hash_drift_is_rejected() {
        let (manifest, inventory, mut downloads) = model_fixture();
        downloads
            .model_files
            .iter_mut()
            .find(|download| download.artifact_id == "pp-ocrv6-tiny-detector-source")
            .unwrap()
            .sha256 = "f".repeat(64);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains("detector-source URL/hash disagrees with authoritative download")
        }));
    }

    #[test]
    fn runtime_artifact_rejects_unsupported_platform() {
        let (mut manifest, inventory, mut downloads) = model_fixture();
        let (mut artifact, download) = runtime_fixture();
        artifact.platforms = vec!["x86_64-linux-android".to_owned()];
        manifest.bundles[0].runtime_artifacts.push(artifact);
        downloads.model_runtime_files.push(download);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("reviewed-runtime is incomplete")));
    }

    #[test]
    fn runtime_artifact_rejects_uppercase_sha256_even_when_download_matches() {
        let (mut manifest, inventory, mut downloads) = model_fixture();
        let (mut artifact, mut download) = runtime_fixture();
        artifact.sha256 = "A".repeat(64);
        download.sha256 = artifact.sha256.clone();
        manifest.bundles[0].runtime_artifacts.push(artifact);
        downloads.model_runtime_files.push(download);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        validate_download_fields(&downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("reviewed-runtime is incomplete")));
        assert!(errors.iter().any(|error| error.contains("runtime download")));
    }

    #[test]
    fn source_and_runtime_role_rules_match_runtime_validation() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles[0].source_artifacts[1].role = "detector".into();
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("required source roles")));

        let (mut manifest, inventory, mut downloads) = model_fixture();
        let (detector, detector_download) = runtime_fixture();
        manifest.bundles[0].runtime_artifacts.push(detector);
        downloads.model_runtime_files.push(detector_download);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("required runtime roles")));

        let (mut recognizer, mut recognizer_download) = runtime_fixture();
        recognizer.id = "reviewed-recognizer-runtime".into();
        recognizer.role = "recognizer-and-dictionary".into();
        recognizer.file_name = "recognizer.onnx".into();
        recognizer.url = "https://example.invalid/recognizer.onnx".into();
        recognizer_download.artifact_id = recognizer.id.clone();
        recognizer_download.repository = "reviewed_recognizer_runtime".into();
        recognizer_download.downloaded_file_path = recognizer.file_name.clone();
        recognizer_download.url = recognizer.url.clone();
        manifest.bundles[0].runtime_artifacts.push(recognizer);
        downloads.model_runtime_files.push(recognizer_download);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(!errors.iter().any(|error| error.contains("runtime roles")));
    }
}
