//! Offline validation for the repository's license policy and inventories.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use toml::Value as TomlValue;
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u64,
    allowed: BTreeSet<String>,
    denied: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Component {
    id: String,
    kind: String,
    status: String,
    included_in_release: bool,
    #[serde(default)]
    manual_only: bool,
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
struct NpmInventory {
    schema_version: u64,
    packages: Vec<NpmPackage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NpmPackage {
    name: String,
    version: String,
    integrity: String,
    license: String,
    source: String,
    scope: String,
    included_in_release: bool,
    #[serde(default)]
    license_file: Option<String>,
    #[serde(default)]
    license_sha256: Option<String>,
    #[serde(default)]
    license_source: Option<String>,
    #[serde(default)]
    copyright: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxDocument {
    spdx_version: String,
    data_license: String,
    #[serde(rename = "SPDXID")]
    spdx_id: String,
    name: String,
    document_namespace: String,
    creation_info: SpdxCreationInfo,
    packages: Vec<SpdxPackage>,
    files: Vec<SpdxFile>,
    relationships: Vec<SpdxRelationship>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxCreationInfo {
    created: String,
    creators: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxPackage {
    #[serde(rename = "SPDXID")]
    spdx_id: String,
    name: String,
    version_info: String,
    download_location: String,
    files_analyzed: bool,
    license_concluded: String,
    license_declared: String,
    copyright_text: String,
    external_refs: Vec<SpdxExternalRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxExternalRef {
    #[serde(rename = "referenceCategory")]
    category: String,
    #[serde(rename = "referenceType")]
    kind: String,
    #[serde(rename = "referenceLocator")]
    locator: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxFile {
    #[serde(rename = "SPDXID")]
    spdx_id: String,
    file_name: String,
    checksums: Vec<SpdxChecksum>,
    license_concluded: String,
    license_info_in_files: Vec<String>,
    copyright_text: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxChecksum {
    algorithm: String,
    checksum_value: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpdxRelationship {
    spdx_element_id: String,
    relationship_type: String,
    related_spdx_element: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConsoleAssetManifest {
    schema_version: u64,
    assets: Vec<ConsoleAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsoleAsset {
    path: String,
    sha256: String,
    bytes: u64,
    mime: String,
    cache: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtManifest {
    version: String,
    api_version: u32,
    source: String,
    license: String,
    targets: BTreeMap<String, OrtTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtTarget {
    asset: String,
    sha256: String,
    library: String,
    library_bytes: u64,
    worker_address_space_overhead_bytes: u64,
    binary_format: String,
    binary_architecture: String,
    load_identity: String,
    library_sha256: String,
    rpaths: Vec<String>,
    system_dependencies: Vec<OrtSystemDependency>,
    companion_dependencies: Vec<OrtCompanionDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtSystemDependency {
    load_name: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrtCompanionDependency {
    load_name: String,
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PdfiumManifest {
    schema_version: u64,
    version: String,
    chromium_build: u64,
    source: String,
    release_download_base: String,
    upstream_source: String,
    license: String,
    distribution_license_note: String,
    required_exports: Vec<String>,
    targets: BTreeMap<String, PdfiumTarget>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PdfiumTarget {
    asset: String,
    archive_size: u64,
    archive_sha256: String,
    library: String,
    library_size: u64,
    library_sha256: String,
    format_pattern: String,
    allowed_dependencies: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FfmpegSource {
    schema_version: u32,
    version: String,
    source_url: String,
    source_bytes: u64,
    source_sha256: String,
    source_date_epoch: u64,
    signature_url: String,
    signature_bytes: u64,
    signature_sha256: String,
    signing_key_url: String,
    signing_key_bytes: u64,
    signing_key_sha256: String,
    signing_key_fingerprint: String,
    license_conclusion: String,
    supported_targets: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FfmpegFixtures {
    schema_version: u32,
    purpose: String,
    distribution: String,
    redistribution: String,
    included_in_artifacts: bool,
    provenance: String,
    fixtures: Vec<FfmpegFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FfmpegFixture {
    format: String,
    url: String,
    bytes: u64,
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
    #[serde(default = "pipeline_model_kind")]
    kind: String,
    availability: String,
    upstream_version: String,
    languages: Vec<String>,
    platforms: Vec<String>,
    runtime_format: String,
    character_set: ModelCharacterSet,
    runtime_artifacts: Vec<ModelRuntimeArtifact>,
    source_artifacts: Vec<ModelArtifact>,
}

fn pipeline_model_kind() -> String {
    "ocr-pipeline".to_owned()
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
    archive_sha256: Option<String>,
    archive_size: Option<u64>,
    archive_member: Option<String>,
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
    pdfium_archives: Vec<NativeDownload>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FixtureDownloadManifest {
    schema_version: u64,
    artifacts: Vec<FixtureDownload>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FixtureDownload {
    artifact_id: String,
    repository: String,
    downloaded_file_path: String,
    url: String,
    allowed_hosts: Vec<String>,
    sha256: String,
    size: u64,
    maximum_redirects: u8,
    license: String,
    manual_only: bool,
    included_in_release: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureCorpus {
    schema_version: u32,
    generator: FixtureGenerator,
    available_formats: Vec<String>,
    fixtures: Vec<CorpusFixture>,
    large_artifacts: Vec<CorpusArtifact>,
    ocr_quality: OcrQuality,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureGenerator {
    path: String,
    version: String,
    source_revision: String,
    sha256: String,
    seed: u64,
    python: String,
    pillow: String,
    freetype: String,
    reference_platform: String,
    pillow_wheel_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFixture {
    id: String,
    format: String,
    scenario: String,
    path: String,
    bytes: u64,
    sha256: String,
    media_type: String,
    license: FixtureLicense,
    provenance: FixtureProvenance,
    expected: FixtureExpected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureLicense {
    spdx: String,
    copyright: String,
    redistribution: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureProvenance {
    kind: String,
    source_url: String,
    author: String,
    acquired_on: String,
    generator: String,
    source_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureExpected {
    outcome: String,
    error_code: String,
    semantic_sha256: String,
    description: String,
    #[serde(default)]
    limit: Option<FixtureLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureLimit {
    option: String,
    failing_value: u64,
    passing_value: u64,
    error_limit: String,
    passing_semantic_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusArtifact {
    id: String,
    purpose: String,
    url: String,
    allowed_hosts: Vec<String>,
    bytes: u64,
    sha256: String,
    maximum_redirects: u8,
    source_revision: String,
    source_container: String,
    source_container_sha256: String,
    license: String,
    license_url: String,
    author: String,
    acquired_on: String,
    redistribution: String,
    manual_only: bool,
    included_in_release: bool,
    repository: String,
    downloaded_file_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OcrQuality {
    license: String,
    copyright: String,
    unicode_normalization: String,
    line_endings: String,
    whitespace_rule: String,
    punctuation_rule: String,
    font_artifact_id: String,
    render: OcrRender,
    training_pollution_statement: String,
    goldens: Vec<OcrGolden>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OcrRender {
    mode: String,
    canvas_width: u32,
    canvas_height: u32,
    font_size: u32,
    origin_x: u32,
    origin_y: u32,
    foreground: u8,
    background: u8,
    png_compress_level: u8,
    layout_engine: String,
    antialiasing: String,
    locale: String,
    dpi_metadata: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OcrGolden {
    fixture_id: String,
    fixture_sha256: String,
    group: String,
    ground_truth_nfc: String,
    codepoints: usize,
    evaluated_characters: usize,
    maximum_cer: f64,
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
    archive_sha256: Option<String>,
    archive_size: Option<u64>,
    archive_member: Option<String>,
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
    #[serde(default)]
    strip_prefix: Option<String>,
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
    let npm_inventory_text =
        read(&root.join("third_party/licenses/npm-inventory.json"), &mut errors);
    let lock_text = read(&root.join("Cargo.lock"), &mut errors);
    let pnpm_lock_text = read(&root.join("pnpm-lock.yaml"), &mut errors);
    let workspace_packages = validate_workspace_metadata(root, &mut errors);

    let policy: Option<Policy> = parse_json("policy.json", &policy_text, &mut errors);
    let inventory: Option<Inventory> = parse_json("inventory.json", &inventory_text, &mut errors);
    let npm_inventory: Option<NpmInventory> =
        parse_json("npm-inventory.json", &npm_inventory_text, &mut errors);

    if let Some(policy) = &policy {
        validate_policy(policy, &mut errors);
        validate_rust_lock(&lock_text, &approvals_text, &workspace_packages, policy, &mut errors);
        if let Some(npm_inventory) = &npm_inventory {
            validate_npm_lock(&pnpm_lock_text, npm_inventory, policy, release, &mut errors);
            validate_npm_release_metadata(root, npm_inventory, &mut errors);
        }
    }
    if let (Some(policy), Some(inventory)) = (&policy, &inventory) {
        validate_inventory(inventory, policy, release, &mut errors);
        validate_existing_manifests(root, inventory, &mut errors);
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn validate_npm_lock(
    lock: &str,
    inventory: &NpmInventory,
    policy: &Policy,
    release: bool,
    errors: &mut Vec<String>,
) {
    if inventory.schema_version != 1 {
        errors.push("unsupported npm inventory schema_version".to_owned());
    }
    let mut locked = BTreeMap::new();
    let mut in_packages = false;
    let mut current: Option<String> = None;
    for line in lock.lines() {
        if line == "packages:" {
            in_packages = true;
            continue;
        }
        if line == "snapshots:" {
            break;
        }
        if !in_packages {
            continue;
        }
        if line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(':') {
            current = Some(line.trim().trim_end_matches(':').trim_matches('\'').to_owned());
        } else if let (Some(key), Some(start)) = (&current, line.find("integrity: ")) {
            let value = line[start + "integrity: ".len()..].trim_end_matches('}').trim().to_owned();
            if locked.insert(key.clone(), value).is_some() {
                errors.push(format!("pnpm lock package {key} is duplicated"));
            }
            current = None;
        }
    }
    let mut reviewed = BTreeSet::new();
    for package in &inventory.packages {
        let key = format!("{}@{}", package.name, package.version);
        if !reviewed.insert(key.clone()) {
            errors.push(format!("npm inventory package {key} is duplicated"));
        }
        match locked.get(&key) {
            Some(integrity) if integrity == &package.integrity => {}
            Some(_) => {
                errors.push(format!("npm package {key} integrity differs from pnpm-lock.yaml"));
            }
            None => errors.push(format!("npm inventory package {key} is orphaned")),
        }
        validate_license_conclusion(&key, &package.license, policy, errors);
        if !package.integrity.starts_with("sha512-") {
            errors.push(format!("npm package {key} must use sha512 integrity"));
        }
        if !package.source.starts_with("https://") {
            errors.push(format!("npm package {key} has no HTTPS source"));
        }
        if !matches!(package.scope.as_str(), "runtime" | "build" | "test") {
            errors.push(format!("npm package {key} has invalid scope"));
        }
        if package.included_in_release != (package.scope == "runtime") {
            errors.push(format!("npm package {key} release flag disagrees with its scope"));
        }
        if release && package.included_in_release && package.license != "MIT" {
            errors.push(format!("released npm package {key} needs an audited notice conclusion"));
        }
    }
    for key in locked.keys() {
        if !reviewed.contains(key) {
            errors.push(format!("pnpm lock package {key} has no npm inventory entry"));
        }
    }
}

const REACT_MIT_SHA256: &str = "da6d3703ed11cbe42bd212c725957c98da23cbff1998c05fa4b3d976d1a58e93";
const REACT_COPYRIGHT: &str = "Copyright (c) Meta Platforms, Inc. and affiliates.";

fn validate_npm_release_metadata(root: &Path, inventory: &NpmInventory, errors: &mut Vec<String>) {
    let mut released = BTreeMap::new();
    let mut checked_license_files = BTreeSet::new();
    for package in &inventory.packages {
        if !package.included_in_release {
            if package.license_file.is_some()
                || package.license_sha256.is_some()
                || package.license_source.is_some()
                || package.copyright.is_some()
            {
                errors.push(format!(
                    "non-release npm package {}@{} has release-only notice metadata",
                    package.name, package.version
                ));
            }
            continue;
        }
        let key = format!("{}@{}", package.name, package.version);
        if released.insert(package.name.as_str(), package).is_some() {
            errors.push(format!("released npm package name {} is duplicated", package.name));
        }
        let fields = [
            ("license_file", package.license_file.as_deref()),
            ("license_sha256", package.license_sha256.as_deref()),
            ("license_source", package.license_source.as_deref()),
            ("copyright", package.copyright.as_deref()),
        ];
        for (field, value) in fields {
            if value.is_none_or(str::is_empty) {
                errors.push(format!("released npm package {key} lacks {field}"));
            }
        }
        if package.license_sha256.as_deref() != Some(REACT_MIT_SHA256) {
            errors.push(format!("released npm package {key} has an unreviewed license hash"));
        }
        if package.copyright.as_deref() != Some(REACT_COPYRIGHT) {
            errors.push(format!("released npm package {key} has unexpected copyright text"));
        }
        let expected_source =
            format!("https://registry.npmjs.org/{0}/-/{0}-{1}.tgz", package.name, package.version);
        if package.license_source.as_deref() != Some(expected_source.as_str()) {
            errors.push(format!(
                "released npm package {key} license source is not its exact tarball"
            ));
        }
        let Some(license_file) = package.license_file.as_deref() else {
            continue;
        };
        if !is_safe_relative_path(license_file)
            || !license_file.starts_with("third_party/licenses/npm/")
        {
            errors.push(format!("released npm package {key} has unsafe license file path"));
            continue;
        }
        if checked_license_files.insert(license_file) {
            match fs::read(root.join(license_file)) {
                Ok(bytes) => validate_release_license_contents(license_file, Some(&bytes), errors),
                Err(_) => validate_release_license_contents(license_file, None, errors),
            }
        }
    }

    let root_build = read(&root.join("BUILD.bazel"), errors);
    validate_release_file_collection(&root_build, &checked_license_files, errors);

    let sbom_text = read(&root.join("third_party/licenses/npm-release.spdx.json"), errors);
    let manifest_text = read(&root.join("web/console/dist/asset-manifest.json"), errors);
    let sbom: Option<SpdxDocument> = parse_json("npm release SPDX SBOM", &sbom_text, errors);
    let manifest: Option<ConsoleAssetManifest> =
        parse_json("console asset manifest", &manifest_text, errors);
    if let (Some(sbom), Some(manifest)) = (&sbom, &manifest) {
        validate_npm_spdx(root, &released, sbom, manifest, errors);
    }
}

fn validate_release_file_collection(
    root_build: &str,
    npm_license_files: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    let Some(name_offset) = root_build.find("name = \"release_license_files\"") else {
        errors.push("root BUILD has no release_license_files authority".to_owned());
        return;
    };
    let Some(srcs_relative) = root_build[name_offset..].find("srcs = [") else {
        errors.push("release_license_files has no literal srcs list".to_owned());
        return;
    };
    let list_start = name_offset + srcs_relative + "srcs = [".len();
    let Some(list_length) = root_build[list_start..].find(']') else {
        errors.push("release_license_files srcs list is unterminated".to_owned());
        return;
    };
    let mut actual = BTreeSet::new();
    for line in root_build[list_start..list_start + list_length].lines() {
        let entry = line.trim().trim_end_matches(',');
        if entry.is_empty() {
            continue;
        }
        let Some(label) = entry.strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
            errors.push("release_license_files must contain only literal labels".to_owned());
            continue;
        };
        let Some(path) = release_label_path(label) else {
            errors.push(format!("release_license_files contains invalid label {label}"));
            continue;
        };
        if !actual.insert(path.clone()) {
            errors.push(format!("release_license_files duplicates {path}"));
        }
    }

    let mut expected = BTreeSet::from([
        "LICENSE".to_owned(),
        "NOTICE".to_owned(),
        "THIRD_PARTY_NOTICES.md".to_owned(),
        "third_party/licenses/npm-release.spdx.json".to_owned(),
    ]);
    expected.extend(npm_license_files.iter().map(|path| (*path).to_owned()));
    for path in expected.difference(&actual) {
        errors.push(format!("release_license_files is missing required {path}"));
    }
    for path in actual.difference(&expected) {
        errors.push(format!("release_license_files has unmanaged entry {path}"));
    }
}

fn release_label_path(label: &str) -> Option<String> {
    if let Some(absolute) = label.strip_prefix("//") {
        let (package, target) = absolute.split_once(':')?;
        if package.is_empty() || target.is_empty() {
            return None;
        }
        let path = format!("{package}/{target}");
        is_safe_relative_path(&path).then_some(path)
    } else {
        is_safe_relative_path(label).then(|| label.to_owned())
    }
}

fn validate_release_license_contents(
    license_file: &str,
    contents: Option<&[u8]>,
    errors: &mut Vec<String>,
) {
    match contents {
        Some(bytes) if sha256_hex(bytes) == REACT_MIT_SHA256 => {}
        Some(_) => errors.push(format!("release license file {license_file} has drifted")),
        None => errors.push(format!("release license file {license_file} is missing")),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the strict SPDX graph and artifact checks remain together as one release invariant"
)]
fn validate_npm_spdx(
    root: &Path,
    released: &BTreeMap<&str, &NpmPackage>,
    sbom: &SpdxDocument,
    manifest: &ConsoleAssetManifest,
    errors: &mut Vec<String>,
) {
    if manifest.schema_version != 1 {
        errors.push("unsupported console asset manifest schemaVersion".to_owned());
    }
    let mut asset_paths = BTreeSet::new();
    let mut app_assets = Vec::new();
    for asset in &manifest.assets {
        if !asset_paths.insert(asset.path.as_str()) {
            errors.push(format!("console asset manifest duplicates {}", asset.path));
        }
        if is_console_app_path(&asset.path) {
            app_assets.push(asset);
        }
    }
    let Some(app) = app_assets.first().copied() else {
        errors.push("console asset manifest has no production app JavaScript".to_owned());
        return;
    };
    if app_assets.len() != 1 {
        errors.push(
            "console asset manifest must contain exactly one production app JavaScript".to_owned(),
        );
    }
    let hash_prefix = app.sha256.get(..16).unwrap_or_default();
    if !is_sha256(&app.sha256)
        || hash_prefix.is_empty()
        || !app.path.contains(hash_prefix)
        || app.mime != "text/javascript; charset=utf-8"
        || app.cache != "immutable"
    {
        errors.push("production app asset has invalid hash, MIME, or cache metadata".to_owned());
    }
    let app_relative = app.path.strip_prefix('/').unwrap_or_default();
    let app_file = root.join("web/console/dist").join(app_relative);
    match fs::read(&app_file) {
        Ok(bytes) => {
            if u64::try_from(bytes.len()).ok() != Some(app.bytes)
                || sha256_hex(&bytes) != app.sha256
            {
                errors.push("production app bytes disagree with the asset manifest".to_owned());
            }
        }
        Err(error) => errors.push(format!("cannot read production app asset: {error}")),
    }

    let expected_name = format!("into-markdown-web-console-{hash_prefix}");
    let expected_namespace = format!(
        "https://github.com/coolplayagent/into-markdown/spdx/web-console/sha256-{}",
        app.sha256
    );
    if sbom.spdx_version != "SPDX-2.3"
        || sbom.data_license != "CC0-1.0"
        || sbom.spdx_id != "SPDXRef-DOCUMENT"
        || sbom.name != expected_name
        || sbom.document_namespace != expected_namespace
        || sbom.creation_info.created != "2026-08-13T00:00:00Z"
        || sbom.creation_info.creators != ["Organization: into-markdown contributors"]
    {
        errors.push("npm release SPDX document metadata is not deterministic authority".to_owned());
    }

    let mut identifiers = BTreeSet::new();
    identifiers.insert(sbom.spdx_id.as_str());
    for id in sbom
        .packages
        .iter()
        .map(|package| package.spdx_id.as_str())
        .chain(sbom.files.iter().map(|file| file.spdx_id.as_str()))
    {
        if !id.starts_with("SPDXRef-") || !identifiers.insert(id) {
            errors.push(format!("npm release SPDX identifier {id} is invalid or duplicated"));
        }
    }

    let package_names: Vec<_> = sbom.packages.iter().map(|package| package.name.as_str()).collect();
    let mut sorted_names = package_names.clone();
    sorted_names.sort_unstable();
    if package_names != sorted_names {
        errors.push("npm release SPDX packages are not deterministically sorted".to_owned());
    }
    let mut sbom_names = BTreeSet::new();
    for package in &sbom.packages {
        if !sbom_names.insert(package.name.as_str()) {
            errors.push(format!("npm release SPDX package {} is duplicated", package.name));
        }
        let Some(inventory) = released.get(package.name.as_str()).copied() else {
            errors.push(format!(
                "npm release SPDX package {} is not a released package",
                package.name
            ));
            continue;
        };
        let expected_id = format!("SPDXRef-Package-{}", package.name);
        let expected_purl = format!("pkg:npm/{}@{}", package.name, package.version_info);
        let external_ref_ok = matches!(package.external_refs.as_slice(), [reference]
            if reference.category == "PACKAGE-MANAGER"
                && reference.kind == "purl"
                && reference.locator == expected_purl);
        if package.spdx_id != expected_id
            || package.version_info != inventory.version
            || package.download_location != inventory.license_source.as_deref().unwrap_or_default()
            || package.files_analyzed
            || package.license_concluded != inventory.license
            || package.license_declared != inventory.license
            || package.copyright_text != inventory.copyright.as_deref().unwrap_or_default()
            || !external_ref_ok
        {
            errors.push(format!(
                "npm release SPDX package {} disagrees with the release inventory",
                package.name
            ));
        }
    }
    if sbom_names != released.keys().copied().collect() {
        errors.push(
            "npm release SPDX packages do not exactly cover released npm inventory".to_owned(),
        );
    }

    let expected_file_name = format!("./web/console/dist{}", app.path);
    let expected_file_id = "SPDXRef-File-console-app";
    if !matches!(sbom.files.as_slice(), [file]
        if file.spdx_id == expected_file_id
            && file.file_name == expected_file_name
            && matches!(file.checksums.as_slice(), [checksum]
                if checksum.algorithm == "SHA256" && checksum.checksum_value == app.sha256)
            && file.license_concluded == "NOASSERTION"
            && file.license_info_in_files == ["MIT"]
            && file.copyright_text == "NOASSERTION")
    {
        errors.push(
            "npm release SPDX file does not describe the exact production app asset".to_owned(),
        );
    }

    let mut relationships = BTreeSet::new();
    for relationship in &sbom.relationships {
        if !relationships.insert(relationship.clone()) {
            errors.push("npm release SPDX relationship is duplicated".to_owned());
        }
        if !identifiers.contains(relationship.spdx_element_id.as_str())
            || !identifiers.contains(relationship.related_spdx_element.as_str())
        {
            errors.push(format!(
                "npm release SPDX relationship {} has a dangling endpoint",
                relationship.relationship_type
            ));
        }
    }
    let mut expected_relationships = BTreeSet::from([SpdxRelationship {
        spdx_element_id: "SPDXRef-DOCUMENT".to_owned(),
        relationship_type: "DESCRIBES".to_owned(),
        related_spdx_element: expected_file_id.to_owned(),
    }]);
    for package in released.keys() {
        expected_relationships.insert(SpdxRelationship {
            spdx_element_id: expected_file_id.to_owned(),
            relationship_type: "GENERATED_FROM".to_owned(),
            related_spdx_element: format!("SPDXRef-Package-{package}"),
        });
    }
    if relationships != expected_relationships {
        errors.push(
            "npm release SPDX relationships do not match the released artifact graph".to_owned(),
        );
    }
}

fn is_console_app_path(path: &str) -> bool {
    let Some(hash) = path.strip_prefix("/assets/app.").and_then(|value| value.strip_suffix(".js"))
    else {
        return false;
    };
    hash.len() == 16
        && hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
        || !notices.contains("third_party/licenses/npm-inventory.json")
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
        if component.manual_only && component.included_in_release {
            errors.push(format!(
                "manual-only component {} cannot be included in a release",
                component.id
            ));
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
    let pdfium_text = read(&root.join("third_party/pdfium/manifest.json"), errors);
    let ffmpeg_text = read(&root.join("third_party/ffmpeg/source.json"), errors);
    let ffmpeg_fixtures_text = read(&root.join("third_party/ffmpeg/fixtures.json"), errors);
    let fixture_corpus_text = read(&root.join("fixtures/manifest.json"), errors);
    let fixture_downloads_text = read(&root.join("fixtures/downloads.json"), errors);
    let ort: Option<OrtManifest> = parse_json("ONNX Runtime manifest", &ort_text, errors);
    let models: Option<ModelManifest> = parse_json("model manifest", &models_text, errors);
    let downloads: Option<DownloadManifest> =
        parse_json("download manifest", &downloads_text, errors);
    let pdfium: Option<PdfiumManifest> = parse_json("PDFium manifest", &pdfium_text, errors);
    let ffmpeg: Option<FfmpegSource> = parse_json("FFmpeg source manifest", &ffmpeg_text, errors);
    let ffmpeg_fixtures: Option<FfmpegFixtures> =
        parse_json("FFmpeg fixture policy", &ffmpeg_fixtures_text, errors);
    let fixture_corpus: Option<FixtureCorpus> =
        parse_json("fixture corpus manifest", &fixture_corpus_text, errors);
    let fixture_downloads: Option<FixtureDownloadManifest> =
        parse_json("fixture download manifest", &fixture_downloads_text, errors);
    if let Some(ffmpeg) = &ffmpeg {
        validate_ffmpeg_source(inventory, ffmpeg, errors);
    }
    if let Some(fixtures) = &ffmpeg_fixtures {
        validate_ffmpeg_fixtures(fixtures, errors);
    }

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
    if let (Some(pdfium), Some(downloads)) = (&pdfium, &downloads) {
        validate_pdfium_manifest(inventory, pdfium, downloads, errors);
    }
    if let Some(downloads) = &fixture_downloads {
        validate_fixture_download_fields(downloads, errors);
    }
    if let (Some(corpus), Some(downloads)) = (&fixture_corpus, &fixture_downloads) {
        validate_fixture_corpus(root, inventory, corpus, downloads, errors);
    }
}

fn validate_fixture_corpus(
    root: &Path,
    inventory: &Inventory,
    corpus: &FixtureCorpus,
    downloads: &FixtureDownloadManifest,
    errors: &mut Vec<String>,
) {
    if corpus.schema_version != 1 {
        errors.push("unsupported fixture corpus schema_version".to_owned());
    }
    validate_fixture_generator(root, &corpus.generator, errors);

    let required_formats = BTreeSet::from([
        "csv", "docx", "feed", "html", "ipynb", "json", "markdown", "pdf", "text", "tsv", "xml",
        "zip",
    ]);
    let formats: BTreeSet<_> = corpus.available_formats.iter().map(String::as_str).collect();
    if formats != required_formats || formats.len() != corpus.available_formats.len() {
        errors.push(
            "fixture corpus available_formats must exactly match converter availability".to_owned(),
        );
    }

    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut fixtures_by_id = BTreeMap::new();
    let mut scenarios: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut declared_files = BTreeSet::new();
    for fixture in &corpus.fixtures {
        if !ids.insert(fixture.id.as_str()) || !is_safe_model_id(&fixture.id) {
            errors.push(format!("fixture {} has a duplicate or unsafe ID", fixture.id));
        }
        fixtures_by_id.insert(fixture.id.as_str(), fixture);
        if !paths.insert(fixture.path.as_str()) || !is_safe_fixture_path(&fixture.path) {
            errors.push(format!("fixture {} has a duplicate or unsafe path", fixture.id));
        }
        if fixture.format != "ocr-image" && !formats.contains(fixture.format.as_str()) {
            errors.push(format!("fixture {} uses an unavailable format", fixture.id));
        }
        if !matches!(
            fixture.scenario.as_str(),
            "normal" | "corrupt" | "limit" | "encrypted" | "nested" | "malicious"
        ) {
            errors.push(format!("fixture {} has unknown scenario", fixture.id));
        }
        scenarios.entry(&fixture.format).or_default().insert(&fixture.scenario);
        validate_fixture_metadata(fixture, errors);
        let relative = Path::new("fixtures").join(&fixture.path);
        declared_files.insert(relative.clone());
        validate_fixture_file(root, fixture, &relative, errors);
    }
    for format in required_formats {
        // PDF and ZIP fixtures are constructed byte-for-byte in their
        // converter/API test modules. This keeps generated binary archives out
        // of the corpus while preserving deterministic normal/corrupt/limit
        // and adversarial contract coverage under the repository license.
        if matches!(format, "pdf" | "zip") {
            continue;
        }
        let present = scenarios.get(format).cloned().unwrap_or_default();
        for required in ["normal", "corrupt", "limit"] {
            if !present.contains(required) {
                errors.push(format!("fixture format {format} lacks required {required} scenario"));
            }
        }
    }
    let actual_files = collect_fixture_files(root, errors);
    for orphan in actual_files.difference(&declared_files) {
        errors.push(format!("orphan fixture file {}", orphan.display()));
    }
    for missing in declared_files.difference(&actual_files) {
        errors.push(format!("declared fixture file {} is missing", missing.display()));
    }

    validate_fixture_artifacts(inventory, &corpus.large_artifacts, downloads, errors);
    validate_ocr_quality(corpus, &fixtures_by_id, errors);
}

fn validate_fixture_generator(root: &Path, generator: &FixtureGenerator, errors: &mut Vec<String>) {
    if generator.path != "fixtures/generate.py"
        || generator.version != "1.0.0"
        || generator.source_revision != "repository-commit"
        || generator.seed != 20_260_813
        || generator.python != "3.13.14"
        || generator.pillow != "11.3.0"
        || generator.freetype != "2.13.3"
        || generator.reference_platform != "macos-11-arm64-cp313"
        || generator.pillow_wheel_sha256
            != "7db51d222548ccfd274e4572fdbf3e810a5e66b00608862f947b163e613b67dd"
        || !is_sha256(&generator.sha256)
    {
        errors.push("fixture generator authority is incomplete".to_owned());
    }
    match fs::read(root.join(&generator.path)) {
        Ok(bytes) if sha256_hex(&bytes) == generator.sha256 => {}
        Ok(_) => errors.push("fixture generator SHA-256 disagrees with manifest".to_owned()),
        Err(error) => errors.push(format!("cannot read fixture generator: {error}")),
    }
}

fn validate_fixture_metadata(fixture: &CorpusFixture, errors: &mut Vec<String>) {
    if fixture.bytes == 0
        || fixture.bytes > 8 * 1024 * 1024
        || !is_sha256(&fixture.sha256)
        || fixture.media_type.trim().is_empty()
        || fixture.license.spdx != "Apache-2.0"
        || fixture.license.copyright != "2026 into-markdown contributors"
        || fixture.license.redistribution.trim().is_empty()
        || fixture.provenance.kind != "repository-generated"
        || !fixture.provenance.source_url.is_empty()
        || fixture.provenance.author != "into-markdown contributors"
        || fixture.provenance.acquired_on != "2026-08-13"
        || fixture.provenance.generator != "fixtures/generate.py@1.0.0"
        || !fixture.provenance.source_sha256.is_empty()
        || fixture.expected.description.trim().is_empty()
    {
        errors
            .push(format!("fixture {} has incomplete provenance or license metadata", fixture.id));
    }
    match fixture.expected.outcome.as_str() {
        "success"
            if fixture.expected.error_code.is_empty()
                && is_sha256(&fixture.expected.semantic_sha256)
                && fixture.expected.limit.is_none() => {}
        "error"
            if !fixture.expected.error_code.is_empty()
                && fixture.expected.semantic_sha256.is_empty()
                && ((fixture.scenario == "limit") == fixture.expected.limit.is_some()) => {}
        _ => errors.push(format!("fixture {} has inconsistent expected semantics", fixture.id)),
    }
    if let Some(limit) = &fixture.expected.limit
        && (fixture.expected.error_code != "resourceLimit"
            || !matches!(
                limit.option.as_str(),
                "max_input_bytes" | "max_nesting_depth" | "max_table_columns"
            )
            || limit.failing_value.checked_add(1) != Some(limit.passing_value)
            || limit.error_limit.trim().is_empty()
            || !is_sha256(&limit.passing_semantic_sha256))
    {
        errors.push(format!("fixture {} has an invalid exact limit contract", fixture.id));
    }
}

fn validate_fixture_file(
    root: &Path,
    fixture: &CorpusFixture,
    relative: &Path,
    errors: &mut Vec<String>,
) {
    let path = root.join(relative);
    let runfiles = env::var_os("TEST_SRCDIR").is_some();
    let metadata = if runfiles { fs::metadata(&path) } else { fs::symlink_metadata(&path) };
    match metadata {
        Ok(metadata) if metadata.is_file() && (runfiles || !metadata.file_type().is_symlink()) => {
            if metadata.len() != fixture.bytes {
                errors.push(format!("fixture {} byte size disagrees with manifest", fixture.id));
            }
            match fs::read(&path) {
                Ok(bytes) if sha256_hex(&bytes) == fixture.sha256 => {}
                Ok(_) => {
                    errors.push(format!("fixture {} SHA-256 disagrees with manifest", fixture.id));
                }
                Err(error) => errors.push(format!("cannot read fixture {}: {error}", fixture.id)),
            }
        }
        Ok(_) => errors.push(format!("fixture {} is not a regular no-follow file", fixture.id)),
        Err(error) => errors.push(format!("cannot stat fixture {}: {error}", fixture.id)),
    }
}

fn collect_fixture_files(root: &Path, errors: &mut Vec<String>) -> BTreeSet<PathBuf> {
    fn visit(
        root: &Path,
        directory: &Path,
        files: &mut BTreeSet<PathBuf>,
        errors: &mut Vec<String>,
    ) {
        let entries = match fs::read_dir(root.join(directory)) {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(format!("cannot scan {}: {error}", directory.display()));
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                errors.push(format!("cannot read entry under {}", directory.display()));
                continue;
            };
            let relative = directory.join(entry.file_name());
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => visit(root, &relative, files, errors),
                Ok(kind) if kind.is_file() && !kind.is_symlink() => {
                    files.insert(relative);
                }
                Ok(kind)
                    if env::var_os("TEST_SRCDIR").is_some()
                        && kind.is_symlink()
                        && fs::metadata(root.join(&relative))
                            .is_ok_and(|target| target.is_file()) =>
                {
                    files.insert(relative);
                }
                Ok(_) => errors
                    .push(format!("fixture corpus contains non-regular {}", relative.display())),
                Err(error) => {
                    errors.push(format!("cannot inspect {}: {error}", relative.display()));
                }
            }
        }
    }
    let mut files = BTreeSet::new();
    visit(root, Path::new("fixtures/small"), &mut files, errors);
    files
}

fn validate_fixture_artifacts(
    inventory: &Inventory,
    artifacts: &[CorpusArtifact],
    downloads: &FixtureDownloadManifest,
    errors: &mut Vec<String>,
) {
    let mut by_id = BTreeMap::new();
    for artifact in artifacts {
        if by_id.insert(artifact.id.as_str(), artifact).is_some() {
            errors.push(format!("duplicate fixture artifact {}", artifact.id));
        }
        let parsed = url::Url::parse(&artifact.url).ok();
        let host = parsed.as_ref().and_then(url::Url::host_str);
        if !is_safe_model_id(&artifact.id)
            || artifact.purpose.trim().is_empty()
            || !is_canonical_https(&artifact.url)
            || artifact.allowed_hosts.len() != 1
            || host != artifact.allowed_hosts.first().map(String::as_str)
            || artifact.bytes == 0
            || artifact.bytes > 64 * 1024 * 1024
            || !is_sha256(&artifact.sha256)
            || artifact.maximum_redirects != 0
            || artifact.source_revision.trim().is_empty()
            || !matches!(artifact.source_container.as_str(), "raw-file" | "tar")
            || artifact.source_container_sha256 != artifact.sha256
            || !is_canonical_https(&artifact.license_url)
            || artifact.author.trim().is_empty()
            || artifact.acquired_on != "2026-08-13"
            || artifact.redistribution.trim().is_empty()
            || !artifact.manual_only
            || artifact.included_in_release
            || !is_safe_model_id(&artifact.repository)
            || !is_safe_fixture_download_file_name(&artifact.downloaded_file_path)
        {
            errors.push(format!("fixture artifact {} has incomplete authority", artifact.id));
        }
        match exact_component(inventory, &artifact.id, errors) {
            Some(component)
                if component.kind == "fixture-input"
                    && component.status == "reviewed"
                    && !component.included_in_release
                    && component.manual_only == artifact.manual_only
                    && component.source.as_deref() == Some(artifact.url.as_str())
                    && component.license.as_deref() == Some(artifact.license.as_str()) => {}
            Some(_) => {
                errors.push(format!("fixture artifact {} disagrees with inventory", artifact.id));
            }
            None => {}
        }
    }
    let download_by_id: BTreeMap<_, _> = downloads
        .artifacts
        .iter()
        .map(|download| (download.artifact_id.as_str(), download))
        .collect();
    if by_id.keys().copied().collect::<BTreeSet<_>>()
        != download_by_id.keys().copied().collect::<BTreeSet<_>>()
    {
        errors.push(
            "fixture corpus artifacts and download authority are not bidirectional".to_owned(),
        );
    }
    for (id, artifact) in by_id {
        if download_by_id.get(id).is_none_or(|download| {
            download.url != artifact.url
                || download.allowed_hosts != artifact.allowed_hosts
                || download.sha256 != artifact.sha256
                || download.size != artifact.bytes
                || download.maximum_redirects != artifact.maximum_redirects
                || download.license != artifact.license
                || download.manual_only != artifact.manual_only
                || download.included_in_release != artifact.included_in_release
                || download.repository != artifact.repository
                || download.downloaded_file_path != artifact.downloaded_file_path
        }) {
            errors.push(format!("fixture artifact {id} disagrees with download authority"));
        }
    }
}

fn validate_ocr_quality(
    corpus: &FixtureCorpus,
    fixtures_by_id: &BTreeMap<&str, &CorpusFixture>,
    errors: &mut Vec<String>,
) {
    let quality = &corpus.ocr_quality;
    if quality.license != "Apache-2.0"
        || quality.copyright != "2026 into-markdown contributors"
        || quality.unicode_normalization != "NFC"
        || quality.line_endings != "LF"
        || quality.whitespace_rule
            != "collapse Unicode whitespace runs, then exclude whitespace from edit distance"
        || quality.punctuation_rule
            != "retain punctuation and compare Unicode scalar values exactly"
        || quality.training_pollution_statement.trim().is_empty()
        || !corpus.large_artifacts.iter().any(|item| item.id == quality.font_artifact_id)
        || quality.render.mode != "L"
        || quality.render.canvas_width != 1800
        || quality.render.canvas_height != 80
        || quality.render.font_size != 42
        || quality.render.origin_x != 20
        || quality.render.origin_y != 9
        || quality.render.foreground != 0
        || quality.render.background != 255
        || quality.render.png_compress_level != 9
        || quality.render.layout_engine != "Pillow ImageFont.Layout.BASIC"
        || quality.render.antialiasing != "FreeType 2.13.3 grayscale rasterization"
        || quality.render.locale != "locale-independent Unicode codepoint input"
        || quality.render.dpi_metadata != "absent"
    {
        errors.push("OCR fixture quality authority is incomplete".to_owned());
    }
    let mut golden_ids = BTreeSet::new();
    let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
    let mut group_text = BTreeMap::<&str, String>::new();
    for golden in &quality.goldens {
        let fixture = fixtures_by_id.get(golden.fixture_id.as_str()).copied();
        let evaluated =
            golden.ground_truth_nfc.chars().filter(|character| !character.is_whitespace()).count();
        let nfc: String = golden.ground_truth_nfc.nfc().collect();
        let expected_threshold = if golden.group == "mixed" { 0.08 } else { 0.05 };
        if !golden_ids.insert(golden.fixture_id.as_str())
            || fixture.is_none()
            || !matches!(golden.group.as_str(), "simplified" | "traditional" | "english" | "mixed")
            || nfc != golden.ground_truth_nfc
            || golden.ground_truth_nfc.contains(['\r', '\n'])
            || golden.codepoints != golden.ground_truth_nfc.chars().count()
            || golden.evaluated_characters != evaluated
            || (golden.maximum_cer - expected_threshold).abs() > f64::EPSILON
        {
            errors.push(format!("OCR golden {} has invalid text or threshold", golden.fixture_id));
        }
        if fixture.is_none_or(|fixture| !valid_ocr_fixture_binding(golden, fixture)) {
            errors.push(format!(
                "OCR golden {} disagrees with its fixture authority",
                golden.fixture_id
            ));
        }
        *groups.entry(&golden.group).or_default() += evaluated;
        group_text.entry(&golden.group).or_default().push_str(&golden.ground_truth_nfc);
    }
    let ocr_fixture_ids: BTreeSet<_> = fixtures_by_id
        .values()
        .filter(|fixture| fixture.format == "ocr-image")
        .map(|fixture| fixture.id.as_str())
        .collect();
    if golden_ids != ocr_fixture_ids {
        errors.push("OCR fixtures and goldens are not bidirectional".to_owned());
    }
    let minimums =
        BTreeMap::from([("simplified", 60), ("traditional", 60), ("english", 150), ("mixed", 100)]);
    if groups.len() != minimums.len()
        || minimums
            .iter()
            .any(|(group, minimum)| groups.get(group).is_none_or(|count| count < minimum))
    {
        errors.push(
            "OCR goldens do not provide the minimum independently authored character coverage"
                .to_owned(),
        );
    }
    let simplified = group_text.get("simplified").map_or("", String::as_str);
    let traditional = group_text.get("traditional").map_or("", String::as_str);
    let mixed = group_text.get("mixed").map_or("", String::as_str);
    if simplified == traditional
        || !simplified.contains(['验', '质', '线', '标', '损'])
        || !traditional.contains(['驗', '質', '線', '標', '損'])
        || !mixed.chars().any(|character| character.is_ascii_alphabetic())
        || !mixed.contains(['验', '线'])
        || !mixed.contains(['轉', '標', '輸'])
    {
        errors.push(
            "OCR script groups do not contain distinct simplified, traditional, and mixed text"
                .to_owned(),
        );
    }
}

fn valid_ocr_fixture_binding(golden: &OcrGolden, fixture: &CorpusFixture) -> bool {
    fixture.format == "ocr-image"
        && fixture.scenario == "normal"
        && fixture.media_type == "image/png"
        && fixture.path.starts_with("small/ocr/")
        && Path::new(&fixture.path).extension() == Some(std::ffi::OsStr::new("png"))
        && fixture.expected.outcome == "success"
        && fixture.expected.error_code.is_empty()
        && fixture.expected.limit.is_none()
        && fixture.expected.semantic_sha256 == sha256_hex(golden.ground_truth_nfc.as_bytes())
        && is_sha256(&golden.fixture_sha256)
        && golden.fixture_sha256 == fixture.sha256
}

fn is_safe_fixture_path(value: &str) -> bool {
    value.starts_with("small/")
        && is_safe_relative_path(value)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'/' | b'.' | b'-' | b'_')
        })
}

fn validate_pdfium_manifest(
    inventory: &Inventory,
    manifest: &PdfiumManifest,
    downloads: &DownloadManifest,
    errors: &mut Vec<String>,
) {
    let Some(component) = exact_component(inventory, "pdfium", errors) else {
        return;
    };
    if component.status != "reviewed"
        || component.kind != "native-runtime"
        || component.version.as_deref() != Some(manifest.version.as_str())
        || component.source.as_deref() != Some(manifest.source.as_str())
        || component.license.as_deref() != Some(manifest.license.as_str())
    {
        errors.push("component pdfium disagrees with its reviewed manifest".to_owned());
    }
    if manifest.schema_version != 1
        || manifest.version != "153.0.7999.0"
        || manifest.chromium_build != 7999
        || manifest.license != "BSD-3-Clause"
        || manifest.source
            != "https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7999"
        || manifest.release_download_base
            != "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7999"
        || manifest.upstream_source
            != "https://pdfium.googlesource.com/pdfium/+/refs/heads/chromium/7999"
        || manifest.distribution_license_note.is_empty()
    {
        errors.push("PDFium manifest is not the reviewed chromium/7999 release".to_owned());
    }

    let actual_exports: BTreeSet<_> =
        manifest.required_exports.iter().map(String::as_str).collect();
    let expected_exports: BTreeSet<_> = PDFIUM_REQUIRED_EXPORTS.iter().copied().collect();
    if manifest.required_exports.len() != expected_exports.len()
        || actual_exports != expected_exports
    {
        errors.push("PDFium manifest must contain the exact reviewed ABI export set".to_owned());
    }

    let expected_targets = pdfium_expected_targets();
    let actual_target_keys: BTreeSet<_> = manifest.targets.keys().map(String::as_str).collect();
    let expected_target_keys: BTreeSet<_> = expected_targets.keys().copied().collect();
    if actual_target_keys != expected_target_keys {
        errors.push("PDFium manifest must contain exactly the four reviewed targets".to_owned());
    }

    let mut downloads_by_target = BTreeMap::new();
    for download in &downloads.pdfium_archives {
        if downloads_by_target.insert(download.target.as_str(), download).is_some() {
            errors.push(format!("duplicate PDFium download target {}", download.target));
        }
    }
    if downloads_by_target.keys().copied().collect::<BTreeSet<_>>() != expected_target_keys {
        errors.push("download manifest PDFium targets do not match reviewed targets".to_owned());
    }

    for (target, expected) in expected_targets {
        let Some(item) = manifest.targets.get(target) else {
            errors.push(format!("PDFium target {target} is missing"));
            continue;
        };
        if item != &expected.artifact
            || !is_sha256(&item.archive_sha256)
            || !is_sha256(&item.library_sha256)
        {
            errors.push(format!("PDFium target {target} differs from the reviewed artifact"));
        }
        let Some(download) = downloads_by_target.get(target) else {
            continue;
        };
        let expected_url = format!("{}/{}", manifest.release_download_base, item.asset);
        if download.repository != expected.repository
            || download.url != expected_url
            || download.sha256 != item.archive_sha256
            || download.strip_prefix.as_deref() != Some("")
        {
            errors.push(format!("PDFium target {target} disagrees with downloads.json"));
        }
    }
}

struct ExpectedPdfiumTarget {
    repository: &'static str,
    artifact: PdfiumTarget,
}

// This reviewed data table intentionally keeps every target's binary identity adjacent.
#[allow(clippy::too_many_lines)]
fn pdfium_expected_targets() -> BTreeMap<&'static str, ExpectedPdfiumTarget> {
    BTreeMap::from([
        (
            "aarch64-apple-darwin",
            ExpectedPdfiumTarget {
                repository: "pdfium_macos_arm64",
                artifact: PdfiumTarget {
                    asset: "pdfium-mac-arm64.tgz".to_owned(),
                    archive_size: 3_453_147,
                    archive_sha256:
                        "e214ee33f22b2204daa765a545aee1e425d88448e6154dac95c6a06206b7437f"
                            .to_owned(),
                    library: "lib/libpdfium.dylib".to_owned(),
                    library_size: 7_191_008,
                    library_sha256:
                        "33c98063af28c0b7cbf8227f4422bf5c15942df2455cf7f0a5dce3dc601d52b0"
                            .to_owned(),
                    format_pattern: "Mach-O 64-bit.*arm64".to_owned(),
                    allowed_dependencies: [
                        "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit",
                        "/System/Library/Frameworks/CoreGraphics.framework/Versions/A/CoreGraphics",
                        "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
                        "/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation",
                        "/usr/lib/libSystem.B.dylib",
                    ]
                    .map(str::to_owned)
                    .into(),
                },
            },
        ),
        (
            "aarch64-unknown-linux-gnu",
            ExpectedPdfiumTarget {
                repository: "pdfium_linux_arm64",
                artifact: PdfiumTarget {
                    asset: "pdfium-linux-arm64.tgz".to_owned(),
                    archive_size: 3_618_464,
                    archive_sha256:
                        "a19862a36e2b2da3c3fb43f0deef45fbbc331f58cd47943782ae4bd9db4c66d9"
                            .to_owned(),
                    library: "lib/libpdfium.so".to_owned(),
                    library_size: 7_867_192,
                    library_sha256:
                        "95a4d8cde3500f57f486478d27795d411b531a14712df08379f4793538a24a88"
                            .to_owned(),
                    format_pattern: "ELF 64-bit.*ARM aarch64".to_owned(),
                    allowed_dependencies: [
                        "libpthread.so.0",
                        "libm.so.6",
                        "libgcc_s.so.1",
                        "libc.so.6",
                        "ld-linux-aarch64.so.1",
                    ]
                    .map(str::to_owned)
                    .into(),
                },
            },
        ),
        (
            "x86_64-pc-windows-msvc",
            ExpectedPdfiumTarget {
                repository: "pdfium_windows_x86_64",
                artifact: PdfiumTarget {
                    asset: "pdfium-win-x64.tgz".to_owned(),
                    archive_size: 3_762_593,
                    archive_sha256:
                        "55329d5cb5de8a379a2fc563106492d7f385a1f795d18970922c71f708f9fbb4"
                            .to_owned(),
                    library: "bin/pdfium.dll".to_owned(),
                    library_size: 7_260_672,
                    library_sha256:
                        "fb898a1f5ace57805834f390407500bdb6ef93eff326a252ad334a8aae809d8e"
                            .to_owned(),
                    format_pattern: "PE32+.*x86-64|coff-x86-64".to_owned(),
                    allowed_dependencies: [
                        "KERNEL32.dll",
                        "ADVAPI32.dll",
                        "GDI32.dll",
                        "USER32.dll",
                    ]
                    .map(str::to_owned)
                    .into(),
                },
            },
        ),
        (
            "x86_64-unknown-linux-gnu",
            ExpectedPdfiumTarget {
                repository: "pdfium_linux_x86_64",
                artifact: PdfiumTarget {
                    asset: "pdfium-linux-x64.tgz".to_owned(),
                    archive_size: 3_675_613,
                    archive_sha256:
                        "c3af580f9df0fef9545b44115bc5ea440f286956b5f231df69fb373b8efc4f69"
                            .to_owned(),
                    library: "lib/libpdfium.so".to_owned(),
                    library_size: 7_669_256,
                    library_sha256:
                        "224f8ece41f7e35891f11c10073b7b7062d7a18e9ef870586162a85c46130f7d"
                            .to_owned(),
                    format_pattern: "ELF 64-bit.*x86-64".to_owned(),
                    allowed_dependencies: [
                        "libpthread.so.0",
                        "libm.so.6",
                        "libgcc_s.so.1",
                        "libc.so.6",
                        "ld-linux-x86-64.so.2",
                    ]
                    .map(str::to_owned)
                    .into(),
                },
            },
        ),
    ])
}

const PDFIUM_REQUIRED_EXPORTS: [&str; 47] = [
    "FPDF_InitLibraryWithConfig",
    "FPDF_DestroyLibrary",
    "FPDF_LoadMemDocument64",
    "FPDF_CloseDocument",
    "FPDF_GetPageCount",
    "FPDF_LoadPage",
    "FPDF_ClosePage",
    "FPDFText_LoadPage",
    "FPDFText_ClosePage",
    "FPDFText_CountChars",
    "FPDFText_GetText",
    "FPDFText_GetUnicode",
    "FPDFText_GetCharBox",
    "FPDFText_GetFontSize",
    "FPDFText_GetFontInfo",
    "FPDFText_GetCharAngle",
    "FPDF_GetPageWidthF",
    "FPDF_GetPageHeightF",
    "FPDFPage_GetRotation",
    "FPDFPage_CountObjects",
    "FPDFPage_GetObject",
    "FPDFPageObj_GetType",
    "FPDFPageObj_GetBounds",
    "FPDFLink_Enumerate",
    "FPDFLink_GetAnnotRect",
    "FPDFLink_GetAction",
    "FPDFAction_GetType",
    "FPDFAction_GetURIPath",
    "FPDFLink_GetDest",
    "FPDFDest_GetDestPageIndex",
    "FPDFLink_LoadWebLinks",
    "FPDFLink_CountWebLinks",
    "FPDFLink_GetURL",
    "FPDFLink_CountRects",
    "FPDFLink_GetRect",
    "FPDFLink_CloseWebLinks",
    "FPDFBitmap_CreateEx",
    "FPDFBitmap_Destroy",
    "FPDFBitmap_GetBuffer",
    "FPDFBitmap_GetFormat",
    "FPDFBitmap_GetHeight",
    "FPDFBitmap_GetStride",
    "FPDFBitmap_GetWidth",
    "FPDFImageObj_GetBitmap",
    "FPDFImageObj_GetImagePixelSize",
    "FPDF_RenderPageBitmap",
    "FPDF_GetLastError",
];

fn validate_ffmpeg_source(inventory: &Inventory, source: &FfmpegSource, errors: &mut Vec<String>) {
    let Some(component) = exact_component(inventory, "ffmpeg", errors) else { return };
    let expected_targets: BTreeSet<_> =
        SUPPORTED_MODEL_TARGETS.iter().map(ToString::to_string).collect();
    let actual_targets: BTreeSet<_> = source.supported_targets.iter().cloned().collect();
    let expected_url = format!("https://ffmpeg.org/releases/ffmpeg-{}.tar.xz", source.version);
    if source.schema_version != 1
        || source.version != "8.1.2"
        || source.source_url != expected_url
        || !is_canonical_https(&source.source_url)
        || source.source_bytes != 11_710_924
        || source.source_sha256
            != "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c"
        || source.source_date_epoch != 1_781_664_539
        || source.signature_url != format!("{expected_url}.asc")
        || source.signature_bytes != 520
        || source.signature_sha256
            != "0a0963fccd70597838073f3e31b20f4a4d8cc2b5e577472c9a5a1f22624246f8"
        || source.signing_key_url != "https://ffmpeg.org/ffmpeg-devel.asc"
        || source.signing_key_bytes != 1_709
        || source.signing_key_sha256
            != "397b3becedcd5a98769967ff1ff8501ddc89f8368b8f766e4701377d7dbaabe5"
        || source.signing_key_fingerprint != "FCF986EA15E6E293A5644F10B4322F04D67658D8"
        || source.license_conclusion != "LGPL-2.1-or-later"
        || actual_targets != expected_targets
        || component.status != "reviewed"
        || component.kind != "native-runtime"
        || component.version.as_deref() != Some(source.version.as_str())
        || component.source.as_deref() != Some(source.source_url.as_str())
        || component.license.as_deref() != Some(source.license_conclusion.as_str())
    {
        errors.push("FFmpeg source/build authority is incomplete or inconsistent".to_owned());
    }
}

fn validate_ffmpeg_fixtures(manifest: &FfmpegFixtures, errors: &mut Vec<String>) {
    let formats: BTreeSet<_> = manifest.fixtures.iter().map(|item| item.format.as_str()).collect();
    if manifest.schema_version != 1
        || manifest.purpose
            != "positive decoder smoke for the production FfmpegRuntime load and normalize path"
        || manifest.distribution != "transient-manual-ci-only"
        || manifest.redistribution != "prohibited-license-unverified"
        || manifest.included_in_artifacts
        || manifest.provenance
            != "FFmpeg project public samples server; individual authorship and redistribution grants are not documented"
        || formats != BTreeSet::from(["flac", "m4a", "mp3", "ogg"])
        || manifest.fixtures.len() != 4
        || manifest.fixtures.iter().any(|item| {
            !item.url.starts_with("https://samples.ffmpeg.org/")
                || item.bytes == 0
                || item.bytes > 1_048_576
                || !is_sha256(&item.sha256)
        })
    {
        errors.push("FFmpeg transient fixture policy is incomplete or distributable".to_owned());
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

fn is_safe_fixture_download_file_name(value: &str) -> bool {
    is_safe_model_file_name(value)
        && value.len() <= 128
        && value.bytes().next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn is_safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path.components().all(|part| matches!(part, std::path::Component::Normal(_)))
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
    validate_native_download_fields(downloads, errors);
}

fn validate_fixture_download_fields(downloads: &FixtureDownloadManifest, errors: &mut Vec<String>) {
    if downloads.schema_version != 1 {
        errors.push("unsupported fixture download manifest schema_version".to_owned());
    }
    let mut repositories = BTreeSet::new();
    let mut fixture_ids = BTreeSet::new();
    for item in &downloads.artifacts {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !fixture_ids.insert(item.artifact_id.as_str()) {
            errors.push(format!("duplicate fixture download artifact {}", item.artifact_id));
        }
        if !is_safe_model_id(&item.artifact_id)
            || !is_safe_model_id(&item.repository)
            || !is_safe_fixture_download_file_name(&item.downloaded_file_path)
            || !is_canonical_https(&item.url)
            || item.allowed_hosts.len() != 1
            || url::Url::parse(&item.url).ok().and_then(|url| url.host_str().map(str::to_owned))
                != item.allowed_hosts.first().cloned()
            || !is_sha256(&item.sha256)
            || item.size == 0
            || item.size > 64 * 1024 * 1024
            || item.maximum_redirects != 0
            || item.license.trim().is_empty()
            || !item.manual_only
            || item.included_in_release
        {
            errors.push(format!("fixture download {} has incomplete fields", item.repository));
        }
    }
    let required = BTreeMap::from([
        (
            "noto-sans-cjk-sc-regular-generator-font",
            ("fixture_noto_cjk_font", "NotoSansCJKsc-Regular.otf"),
        ),
        (
            "ppocrv6-tiny-recognizer-onnx-quality",
            ("fixture_ppocrv6_tiny_recognizer_onnx", "PP-OCRv6_tiny_rec_onnx_infer.tar"),
        ),
    ]);
    if fixture_ids != required.keys().copied().collect() {
        errors.push("fixture download authority has missing or unexpected artifacts".to_owned());
    }
    for item in &downloads.artifacts {
        if required.get(item.artifact_id.as_str()).is_none_or(|(repository, path)| {
            item.repository != *repository || item.downloaded_file_path != *path
        }) {
            errors.push(format!(
                "fixture download {} has an unexpected portable repository path",
                item.artifact_id
            ));
        }
    }
}

fn validate_native_download_fields(downloads: &DownloadManifest, errors: &mut Vec<String>) {
    let mut repositories: BTreeSet<&str> = downloads
        .model_files
        .iter()
        .map(|item| item.repository.as_str())
        .chain(downloads.model_runtime_files.iter().map(|item| item.repository.as_str()))
        .collect();
    for item in &downloads.native_archives {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !SUPPORTED_MODEL_TARGETS.contains(&item.target.as_str())
            || !is_safe_model_id(&item.repository)
            || item
                .strip_prefix
                .as_deref()
                .is_some_and(|value| !value.is_empty() && !is_safe_model_file_name(value))
            || !is_canonical_https(&item.url)
            || !is_sha256(&item.sha256)
        {
            errors.push(format!("native download {} has incomplete fields", item.repository));
        }
    }
    for item in &downloads.pdfium_archives {
        if !repositories.insert(item.repository.as_str()) {
            errors.push(format!("duplicate download repository {}", item.repository));
        }
        if !SUPPORTED_MODEL_TARGETS.contains(&item.target.as_str())
            || !is_safe_model_id(&item.repository)
            || item.strip_prefix.as_deref() != Some("")
            || !is_canonical_https(&item.url)
            || !is_sha256(&item.sha256)
        {
            errors.push(format!("PDFium download {} has incomplete fields", item.repository));
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
        || manifest.api_version == 0
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
        if !ort_target_audit_is_valid(target, asset) {
            errors.push(format!("ONNX Runtime target {target} has invalid audited fields"));
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
            || download.strip_prefix.as_deref() != expected_strip_prefix
        {
            errors.push(format!(
                "ONNX Runtime target {target} disagrees with authoritative download {repository}"
            ));
        }
    }
}

fn ort_target_audit_is_valid(target: &str, asset: &OrtTarget) -> bool {
    is_safe_model_file_name(&asset.asset)
        && is_sha256(&asset.sha256)
        && is_safe_relative_path(&asset.library)
        && asset.library_bytes != 0
        && asset.library_bytes <= 512 * 1024 * 1024
        && (256 * 1024 * 1024..=2 * 1024 * 1024 * 1024 * 1024)
            .contains(&asset.worker_address_space_overhead_bytes)
        && matches!(asset.binary_format.as_str(), "elf" | "mach-o" | "pe")
        && matches!(asset.binary_architecture.as_str(), "aarch64" | "x86_64")
        && match asset.binary_format.as_str() {
            "elf" | "pe" => is_safe_model_file_name(&asset.load_identity),
            "mach-o" => {
                asset.load_identity.strip_prefix("@rpath/").is_some_and(is_safe_model_file_name)
            }
            _ => false,
        }
        && is_sha256(&asset.library_sha256)
        && asset.rpaths.len() <= 128
        && !asset.system_dependencies.is_empty()
        && !asset.system_dependencies.iter().any(|dependency| {
            dependency.load_name.is_empty()
                || dependency.path.as_ref().is_some_and(String::is_empty)
        })
        && asset.companion_dependencies.is_empty()
        && !asset.companion_dependencies.iter().any(|dependency| {
            dependency.load_name.is_empty()
                || !is_safe_relative_path(&dependency.path)
                || !is_sha256(&dependency.sha256)
        })
        && ort_dependencies_are_system_only(target, &asset.system_dependencies)
}

fn ort_dependencies_are_system_only(target: &str, dependencies: &[OrtSystemDependency]) -> bool {
    let mut unique = BTreeSet::new();
    dependencies.iter().all(|dependency| {
        let normalized = if target == "x86_64-pc-windows-msvc" {
            dependency.load_name.to_ascii_lowercase()
        } else {
            dependency.load_name.clone()
        };
        unique.insert(normalized)
            && if target == "aarch64-apple-darwin" {
                let Some(expected_path) = dependency.path.as_deref() else {
                    return false;
                };
                let path = Path::new(expected_path);
                expected_path == dependency.load_name
                    && (expected_path.starts_with("/System/Library/")
                        || expected_path.starts_with("/usr/lib/"))
                    && path.components().all(|component| {
                        matches!(
                            component,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
            } else {
                dependency.path.is_none() && is_safe_model_file_name(&dependency.load_name)
            }
    })
}

fn validate_model_manifest(
    inventory: &Inventory,
    manifest: &ModelManifest,
    downloads: &DownloadManifest,
    errors: &mut Vec<String>,
) {
    if !matches!(manifest.schema_version, 1 | 2) {
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
        let required_roles = match bundle.kind.as_str() {
            "ocr-pipeline" => BTreeSet::from(["detector", "recognizer-and-dictionary"]),
            "recognizer-component" => BTreeSet::from(["recognizer-and-dictionary"]),
            _ => BTreeSet::new(),
        };
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
    if !is_safe_model_id(&bundle.id)
        || !matches!(bundle.kind.as_str(), "ocr-pipeline" | "recognizer-component")
    {
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
            || match (&artifact.archive_sha256, artifact.archive_size, &artifact.archive_member) {
                (None, None, None) => false,
                (Some(hash), Some(size), Some(member)) => {
                    !is_sha256(hash)
                        || size <= artifact.size
                        || member != "PP-OCRv6_tiny_rec_onnx_infer/inference.onnx"
                }
                _ => true,
            }
        {
            errors.push(format!("runtime model artifact {} is incomplete", artifact.id));
        }
        match runtime_downloads.get(artifact.id.as_str()) {
            Some(download)
                if download.downloaded_file_path == artifact.file_name
                    && download.url == artifact.url
                    && download.sha256 == artifact.sha256
                    && download.size == artifact.size
                    && download.archive_sha256 == artifact.archive_sha256
                    && download.archive_size == artifact.archive_size
                    && download.archive_member == artifact.archive_member => {}
            _ => errors.push(format!(
                "runtime model artifact {} disagrees with authoritative download",
                artifact.id
            )),
        }
    }
    let expected_runtime_roles = match bundle.kind.as_str() {
        "ocr-pipeline" => BTreeSet::from(["detector", "recognizer-and-dictionary"]),
        "recognizer-component" => BTreeSet::from(["recognizer", "character-table"]),
        _ => BTreeSet::new(),
    };
    if !bundle.runtime_artifacts.is_empty() && runtime_roles != expected_runtime_roles {
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

    fn fixture_authorities() -> (PathBuf, FixtureCorpus, FixtureDownloadManifest, Inventory) {
        let root = repository_root().expect("repository root");
        let parse = |path: &str| fs::read_to_string(root.join(path)).expect(path);
        (
            root.clone(),
            serde_json::from_str(&parse("fixtures/manifest.json")).expect("fixture corpus"),
            serde_json::from_str(&parse("fixtures/downloads.json")).expect("fixture downloads"),
            serde_json::from_str(&parse("third_party/licenses/inventory.json")).expect("inventory"),
        )
    }

    fn assert_download_mutation_rejected(mutator: impl FnOnce(&mut FixtureDownload)) {
        let (_, corpus, mut downloads, inventory) = fixture_authorities();
        let mut baseline = Vec::new();
        validate_fixture_artifacts(&inventory, &corpus.large_artifacts, &downloads, &mut baseline);
        validate_fixture_download_fields(&downloads, &mut baseline);
        assert!(baseline.is_empty(), "baseline fixture downloads: {baseline:?}");
        mutator(&mut downloads.artifacts[0]);
        let mut errors = Vec::new();
        validate_fixture_artifacts(&inventory, &corpus.large_artifacts, &downloads, &mut errors);
        validate_fixture_download_fields(&downloads, &mut errors);
        assert!(!errors.is_empty(), "mutated fixture download was accepted");
    }

    fn assert_ocr_mutation_rejected(mutator: impl FnOnce(&mut CorpusFixture, &mut OcrGolden)) {
        let (root, mut corpus, downloads, inventory) = fixture_authorities();
        let fixture_id = corpus.ocr_quality.goldens[0].fixture_id.clone();
        let fixture = corpus
            .fixtures
            .iter_mut()
            .find(|fixture| fixture.id == fixture_id)
            .expect("OCR fixture");
        let golden = &mut corpus.ocr_quality.goldens[0];
        mutator(fixture, golden);
        let mut errors = Vec::new();
        validate_fixture_corpus(&root, &inventory, &corpus, &downloads, &mut errors);
        assert!(!errors.is_empty(), "mutated OCR authority was accepted");
    }

    fn planned_component(id: &str) -> Component {
        Component {
            id: id.to_owned(),
            kind: "placeholder".to_owned(),
            status: "planned".to_owned(),
            included_in_release: false,
            manual_only: false,
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
            manual_only: false,
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
            pdfium_archives: vec![],
        }
    }

    fn bundle(id: &str, version: &str, source_artifacts: Vec<ModelArtifact>) -> ModelBundle {
        ModelBundle {
            id: id.to_owned(),
            kind: "ocr-pipeline".to_owned(),
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
            pdfium_archives: vec![],
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
            archive_sha256: None,
            archive_size: None,
            archive_member: None,
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
            archive_sha256: None,
            archive_size: None,
            archive_member: None,
            sha256: artifact.sha256.clone(),
            size: artifact.size,
        };
        (artifact, download)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "four-platform authority fixture is intentionally explicit"
    )]
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
                OrtTarget {
                    asset: asset.to_owned(),
                    sha256: sha256.clone(),
                    library: "lib/libonnxruntime.so".to_owned(),
                    library_bytes: 1024,
                    worker_address_space_overhead_bytes: 512 * 1024 * 1024,
                    binary_format: if target == "aarch64-apple-darwin" {
                        "mach-o"
                    } else if target == "x86_64-pc-windows-msvc" {
                        "pe"
                    } else {
                        "elf"
                    }
                    .to_owned(),
                    binary_architecture: if target.starts_with("aarch64") {
                        "aarch64"
                    } else {
                        "x86_64"
                    }
                    .to_owned(),
                    load_identity: if target == "aarch64-apple-darwin" {
                        "@rpath/libonnxruntime.1.dylib"
                    } else if target == "x86_64-pc-windows-msvc" {
                        "onnxruntime.dll"
                    } else {
                        "libonnxruntime.so.1"
                    }
                    .to_owned(),
                    library_sha256: marker.to_string().repeat(64),
                    rpaths: if target == "aarch64-apple-darwin" {
                        vec!["@loader_path".to_owned()]
                    } else if target.ends_with("linux-gnu") {
                        vec!["$ORIGIN".to_owned()]
                    } else {
                        Vec::new()
                    },
                    system_dependencies: vec![OrtSystemDependency {
                        load_name: if target == "aarch64-apple-darwin" {
                            "/usr/lib/libSystem.B.dylib"
                        } else if target == "x86_64-pc-windows-msvc" {
                            "KERNEL32.dll"
                        } else {
                            "libc.so.6"
                        }
                        .to_owned(),
                        path: (target == "aarch64-apple-darwin")
                            .then(|| "/usr/lib/libSystem.B.dylib".to_owned()),
                    }],
                    companion_dependencies: Vec::new(),
                },
            );
            native_archives.push(NativeDownload {
                target: target.to_owned(),
                repository: repository.to_owned(),
                url: format!(
                    "https://github.com/microsoft/onnxruntime/releases/download/v{version}/{asset}"
                ),
                sha256,
                strip_prefix: Some(
                    asset
                        .strip_suffix(".tgz")
                        .or_else(|| asset.strip_suffix(".zip"))
                        .unwrap()
                        .to_owned(),
                ),
            });
        }
        let source = format!("https://github.com/microsoft/onnxruntime/releases/tag/v{version}");
        let manifest = OrtManifest {
            version: version.to_owned(),
            api_version: 29,
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
                manual_only: false,
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
                pdfium_archives: vec![],
            },
        )
    }

    fn pdfium_fixture() -> (PdfiumManifest, Inventory, DownloadManifest) {
        let source = "https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7999";
        let release_download_base =
            "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7999";
        let mut targets = BTreeMap::new();
        let mut pdfium_archives = Vec::new();
        for (target, expected) in pdfium_expected_targets() {
            pdfium_archives.push(NativeDownload {
                target: target.to_owned(),
                repository: expected.repository.to_owned(),
                url: format!("{release_download_base}/{}", expected.artifact.asset),
                sha256: expected.artifact.archive_sha256.clone(),
                strip_prefix: Some(String::new()),
            });
            targets.insert(target.to_owned(), expected.artifact);
        }
        (
            PdfiumManifest {
                schema_version: 1,
                version: "153.0.7999.0".to_owned(),
                chromium_build: 7999,
                source: source.to_owned(),
                release_download_base: release_download_base.to_owned(),
                upstream_source:
                    "https://pdfium.googlesource.com/pdfium/+/refs/heads/chromium/7999".to_owned(),
                license: "BSD-3-Clause".to_owned(),
                distribution_license_note: "preserve all notices".to_owned(),
                required_exports: PDFIUM_REQUIRED_EXPORTS.map(str::to_owned).into(),
                targets,
            },
            Inventory {
                schema_version: 1,
                components: vec![Component {
                    id: "pdfium".to_owned(),
                    kind: "native-runtime".to_owned(),
                    status: "reviewed".to_owned(),
                    included_in_release: false,
                    manual_only: false,
                    version: Some("153.0.7999.0".to_owned()),
                    source: Some(source.to_owned()),
                    license: Some("BSD-3-Clause".to_owned()),
                    obligations: Some("preserve all notices".to_owned()),
                }],
            },
            DownloadManifest {
                schema_version: 1,
                model_files: vec![],
                model_runtime_files: vec![],
                native_archives: vec![],
                pdfium_archives,
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
    fn boost_license_approval_is_exact_and_rejects_nearby_identifiers() {
        let mut policy = policy();
        policy.allowed.insert("BSL-1.0".to_owned());
        let mut errors = Vec::new();
        validate_license_conclusion("clipper2-rust", "BSL-1.0", &policy, &mut errors);
        assert!(errors.is_empty());

        for invalid in ["BSL", "BSL-1", "BSL-1.0+", "Boost-1.0"] {
            validate_license_conclusion("lookalike", invalid, &policy, &mut errors);
        }
        assert_eq!(
            errors.iter().filter(|error| error.contains("non-allowed concluded license")).count(),
            4
        );
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
            api_version: 29,
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
                "strip_prefix" => download.strip_prefix = Some("wrong-prefix".into()),
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

        let (mut manifest, inventory, downloads) = ort_fixture();
        manifest.targets.get_mut("aarch64-apple-darwin").unwrap().system_dependencies = vec![
            OrtSystemDependency { load_name: "relative.dylib".into(), path: None },
            OrtSystemDependency { load_name: "relative.dylib".into(), path: None },
        ];
        manifest.targets.get_mut("x86_64-pc-windows-msvc").unwrap().load_identity =
            "../onnxruntime.dll".into();
        let mut errors = Vec::new();
        validate_ort_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains("aarch64-apple-darwin") && error.contains("invalid audited fields")
        }));
        assert!(errors.iter().any(|error| {
            error.contains("x86_64-pc-windows-msvc") && error.contains("invalid audited fields")
        }));
    }

    #[test]
    fn pdfium_manifest_rejects_abi_artifact_and_download_drift() {
        let (manifest, inventory, downloads) = pdfium_fixture();
        let mut errors = Vec::new();
        validate_pdfium_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let mut changed = manifest.clone();
        changed.required_exports.pop();
        changed.targets.get_mut("aarch64-apple-darwin").unwrap().library_size += 1;
        let mut changed_downloads = downloads;
        changed_downloads
            .pdfium_archives
            .iter_mut()
            .find(|download| download.target == "aarch64-apple-darwin")
            .unwrap()
            .url
            .push_str(".wrong");
        let mut errors = Vec::new();
        validate_pdfium_manifest(&inventory, &changed, &changed_downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("exact reviewed ABI export set")));
        assert!(errors.iter().any(|error| error.contains("reviewed artifact")));
        assert!(errors.iter().any(|error| error.contains("disagrees with downloads.json")));
    }

    #[test]
    fn pdfium_manifest_rejects_unknown_fields() {
        let mut errors = Vec::new();
        let parsed: Option<PdfiumManifest> =
            parse_json("PDFium manifest", r#"{"schema_version":1,"unexpected":true}"#, &mut errors);
        assert!(parsed.is_none());
        assert!(errors.iter().any(|error| error.contains("unknown field")));
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

    fn npm_package(license: &str) -> NpmPackage {
        NpmPackage {
            name: "example".into(),
            version: "1.0.0".into(),
            integrity: "sha512-abc".into(),
            license: license.into(),
            source: "https://example.invalid/source".into(),
            scope: "test".into(),
            included_in_release: false,
            license_file: None,
            license_sha256: None,
            license_source: None,
            copyright: None,
        }
    }

    #[test]
    fn npm_lock_addition_and_inventory_orphan_are_rejected() {
        let inventory = NpmInventory { schema_version: 1, packages: vec![] };
        let lock = "packages:\n\n  example@1.0.0:\n    resolution: {integrity: sha512-abc}\n\nsnapshots:\n";
        let mut errors = Vec::new();
        validate_npm_lock(lock, &inventory, &policy(), false, &mut errors);
        assert!(errors.iter().any(|error| error.contains("has no npm inventory")));

        let inventory = NpmInventory { schema_version: 1, packages: vec![npm_package("MIT")] };
        let mut errors = Vec::new();
        validate_npm_lock("packages:\n\nsnapshots:\n", &inventory, &policy(), false, &mut errors);
        assert!(errors.iter().any(|error| error.contains("is orphaned")));
    }

    #[test]
    fn npm_denied_license_is_rejected() {
        let inventory =
            NpmInventory { schema_version: 1, packages: vec![npm_package("GPL-3.0-only")] };
        let lock = "packages:\n\n  example@1.0.0:\n    resolution: {integrity: sha512-abc}\n\nsnapshots:\n";
        let mut errors = Vec::new();
        validate_npm_lock(lock, &inventory, &policy(), true, &mut errors);
        assert!(errors.iter().any(|error| error.contains("denied concluded license")));
    }

    #[test]
    fn release_license_file_deletion_and_drift_are_rejected() {
        let root = repository_root().expect("repository root");
        let license = fs::read(root.join("third_party/licenses/npm/react-MIT.txt"))
            .expect("checked release license");
        let mut errors = Vec::new();
        validate_release_license_contents("react-MIT.txt", Some(&license), &mut errors);
        assert!(errors.is_empty());

        validate_release_license_contents("react-MIT.txt", None, &mut errors);
        validate_release_license_contents("react-MIT.txt", Some(b"changed"), &mut errors);
        assert!(errors.iter().any(|error| error.contains("is missing")));
        assert!(errors.iter().any(|error| error.contains("has drifted")));
    }

    #[test]
    fn release_file_collection_rejects_missing_sbom_license_and_unmanaged_entries() {
        let licenses = BTreeSet::from(["third_party/licenses/npm/react-MIT.txt"]);
        let valid = r#"
filegroup(
    name = "release_license_files",
    srcs = [
        "LICENSE",
        "NOTICE",
        "THIRD_PARTY_NOTICES.md",
        "//third_party/licenses:npm-release.spdx.json",
        "//third_party/licenses:npm/react-MIT.txt",
    ],
)
"#;
        let mut errors = Vec::new();
        validate_release_file_collection(valid, &licenses, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let missing_sbom =
            valid.replace("        \"//third_party/licenses:npm-release.spdx.json\",\n", "");
        let mut errors = Vec::new();
        validate_release_file_collection(&missing_sbom, &licenses, &mut errors);
        assert!(errors.iter().any(|error| error.contains("npm-release.spdx.json")));

        let missing_license =
            valid.replace("        \"//third_party/licenses:npm/react-MIT.txt\",\n", "");
        let mut errors = Vec::new();
        validate_release_file_collection(&missing_license, &licenses, &mut errors);
        assert!(errors.iter().any(|error| error.contains("react-MIT.txt")));

        let unmanaged = valid.replace(
            "        \"LICENSE\",\n",
            "        \"LICENSE\",\n        \"unexpected.txt\",\n",
        );
        let mut errors = Vec::new();
        validate_release_file_collection(&unmanaged, &licenses, &mut errors);
        assert!(errors.iter().any(|error| error.contains("unmanaged entry")));
    }

    #[test]
    fn npm_spdx_deletion_drift_duplicate_and_dangling_relationship_are_rejected() {
        let root = repository_root().expect("repository root");
        let inventory: NpmInventory = serde_json::from_str(
            &fs::read_to_string(root.join("third_party/licenses/npm-inventory.json"))
                .expect("npm inventory"),
        )
        .expect("valid npm inventory");
        let sbom: SpdxDocument = serde_json::from_str(
            &fs::read_to_string(root.join("third_party/licenses/npm-release.spdx.json"))
                .expect("npm SPDX"),
        )
        .expect("valid npm SPDX");
        let manifest: ConsoleAssetManifest = serde_json::from_str(
            &fs::read_to_string(root.join("web/console/dist/asset-manifest.json"))
                .expect("asset manifest"),
        )
        .expect("valid asset manifest");
        let released = inventory
            .packages
            .iter()
            .filter(|package| package.included_in_release)
            .map(|package| (package.name.as_str(), package))
            .collect();

        let mut errors = Vec::new();
        validate_npm_spdx(&root, &released, &sbom, &manifest, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let mut duplicate = sbom.clone();
        duplicate.packages[1].spdx_id = duplicate.packages[0].spdx_id.clone();
        let mut errors = Vec::new();
        validate_npm_spdx(&root, &released, &duplicate, &manifest, &mut errors);
        assert!(errors.iter().any(|error| error.contains("duplicated")));

        let mut dangling = sbom.clone();
        dangling.relationships[1].related_spdx_element = "SPDXRef-absent".to_owned();
        let mut errors = Vec::new();
        validate_npm_spdx(&root, &released, &dangling, &manifest, &mut errors);
        assert!(errors.iter().any(|error| error.contains("dangling endpoint")));

        let mut deleted = sbom.clone();
        deleted.packages.remove(0);
        let mut errors = Vec::new();
        validate_npm_spdx(&root, &released, &deleted, &manifest, &mut errors);
        assert!(errors.iter().any(|error| error.contains("do not exactly cover")));

        let mut drifted = sbom.clone();
        drifted.files[0].checksums[0].checksum_value = "f".repeat(64);
        let mut errors = Vec::new();
        validate_npm_spdx(&root, &released, &drifted, &manifest, &mut errors);
        assert!(errors.iter().any(|error| error.contains("exact production app asset")));
    }

    #[test]
    fn fixture_schema_rejects_unknown_expected_fields() {
        let value = r#"{
            "outcome":"error",
            "error_code":"malformed",
            "semantic_sha256":"",
            "description":"damaged input",
            "unexpected":true
        }"#;
        assert!(serde_json::from_str::<FixtureExpected>(value).is_err());
    }

    #[test]
    fn fixture_paths_are_portable_and_normalized() {
        assert!(is_safe_fixture_path("small/text/normal.txt"));
        assert!(is_safe_fixture_path("small/text/ss.txt"));
        assert!(!is_safe_fixture_path("../small/text/normal.txt"));
        assert!(!is_safe_fixture_path("small\\text\\normal.txt"));
        assert!(!is_safe_fixture_path("small/text/e\u{301}.txt"));
        assert!(!is_safe_fixture_path("small/C:/device.txt"));
        for non_ascii in ["ß", "Σ", "σ", "ς"] {
            assert!(!is_safe_fixture_path(&format!("small/text/{non_ascii}.txt")));
        }
    }

    #[test]
    fn ocr_goldens_and_png_fixtures_are_strictly_bidirectional() {
        let (root, mut corpus, downloads, inventory) = fixture_authorities();
        let mut baseline = Vec::new();
        validate_fixture_corpus(&root, &inventory, &corpus, &downloads, &mut baseline);
        assert!(baseline.is_empty(), "baseline fixture corpus: {baseline:?}");

        corpus.ocr_quality.goldens[0].ground_truth_nfc.push('x');
        let mut errors = Vec::new();
        validate_fixture_corpus(&root, &inventory, &corpus, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("OCR golden")));

        let (root, mut corpus, downloads, inventory) = fixture_authorities();
        let fixture_id = corpus.ocr_quality.goldens[0].fixture_id.clone();
        corpus
            .fixtures
            .iter_mut()
            .find(|fixture| fixture.id == fixture_id)
            .expect("OCR fixture")
            .sha256 = "f".repeat(64);
        let mut errors = Vec::new();
        validate_fixture_corpus(&root, &inventory, &corpus, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("SHA-256 disagrees")));
        assert!(errors.iter().any(|error| error.contains("fixture authority")));

        let (root, mut corpus, downloads, inventory) = fixture_authorities();
        corpus.ocr_quality.goldens.pop();
        let mut errors = Vec::new();
        validate_fixture_corpus(&root, &inventory, &corpus, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("not bidirectional")));
    }

    #[test]
    fn ocr_fixture_contract_rejects_each_bound_metadata_or_hash_drift() {
        assert_ocr_mutation_rejected(|fixture, _| fixture.format = "text".into());
        assert_ocr_mutation_rejected(|fixture, _| fixture.scenario = "corrupt".into());
        assert_ocr_mutation_rejected(|fixture, _| fixture.media_type = "image/jpeg".into());
        assert_ocr_mutation_rejected(|fixture, _| {
            fixture.expected.semantic_sha256 = "f".repeat(64);
        });
        assert_ocr_mutation_rejected(|_, golden| golden.fixture_sha256 = "f".repeat(64));
    }

    #[test]
    fn fixture_download_authority_rejects_every_bound_field_mutation() {
        assert_download_mutation_rejected(|item| item.allowed_hosts[0] = "invalid.example".into());
        assert_download_mutation_rejected(|item| item.url.push_str("?drift=1"));
        assert_download_mutation_rejected(|item| item.maximum_redirects = 1);
        assert_download_mutation_rejected(|item| item.size -= 1);
        assert_download_mutation_rejected(|item| item.sha256 = "f".repeat(64));
        assert_download_mutation_rejected(|item| item.license = "MIT".into());
        assert_download_mutation_rejected(|item| item.manual_only = false);
        assert_download_mutation_rejected(|item| item.included_in_release = true);
        assert_download_mutation_rejected(|item| item.repository.push_str("_drift"));
        assert_download_mutation_rejected(|item| item.downloaded_file_path.push_str(".drift"));
    }

    #[test]
    fn fixture_inventory_manual_and_release_flags_are_bound() {
        let (_, corpus, downloads, mut inventory) = fixture_authorities();
        let component = inventory
            .components
            .iter_mut()
            .find(|component| component.id == corpus.large_artifacts[0].id)
            .expect("fixture inventory component");
        component.manual_only = false;
        component.included_in_release = true;
        let mut errors = Vec::new();
        validate_fixture_artifacts(&inventory, &corpus.large_artifacts, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("disagrees with inventory")));
    }

    #[test]
    fn fixture_limit_requires_adjacent_boundaries_and_hash() {
        let fixture = CorpusFixture {
            id: "text-limit".into(),
            format: "text".into(),
            scenario: "limit".into(),
            path: "small/text/limit.txt".into(),
            bytes: 1,
            sha256: "a".repeat(64),
            media_type: "text/plain".into(),
            license: FixtureLicense {
                spdx: "Apache-2.0".into(),
                copyright: "2026 into-markdown contributors".into(),
                redistribution: "permitted".into(),
            },
            provenance: FixtureProvenance {
                kind: "repository-generated".into(),
                source_url: String::new(),
                author: "into-markdown contributors".into(),
                acquired_on: "2026-08-13".into(),
                generator: "fixtures/generate.py@1.0.0".into(),
                source_sha256: String::new(),
            },
            expected: FixtureExpected {
                outcome: "error".into(),
                error_code: "resourceLimit".into(),
                semantic_sha256: String::new(),
                description: "exact input boundary".into(),
                limit: Some(FixtureLimit {
                    option: "max_input_bytes".into(),
                    failing_value: 10,
                    passing_value: 12,
                    error_limit: "max_input_bytes".into(),
                    passing_semantic_sha256: "not-a-hash".into(),
                }),
            },
        };
        let mut errors = Vec::new();
        validate_fixture_metadata(&fixture, &mut errors);
        assert!(errors.iter().any(|error| error.contains("exact limit contract")));
    }
}
