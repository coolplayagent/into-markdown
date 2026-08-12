//! Offline validation for the repository's license policy and inventories.

use base64::Engine as _;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
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
struct OrtManifest {
    version: String,
    source: String,
    license: String,
    targets: BTreeMap<String, OrtTarget>,
}

#[derive(Debug, Deserialize)]
struct OrtTarget {
    asset: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct ModelManifest {
    bundles: Vec<ModelBundle>,
}

#[derive(Debug, Deserialize)]
struct ModelBundle {
    upstream_version: String,
    source_artifacts: Vec<ModelArtifact>,
}

#[derive(Debug, Deserialize)]
struct ModelArtifact {
    id: String,
    url: String,
    sha256: String,
    license: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Download {
    url: String,
    sha256: String,
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
    let module_text = read(&root.join("MODULE.bazel"), errors);
    let ort: Option<OrtManifest> = parse_json("ONNX Runtime manifest", &ort_text, errors);
    let models: Option<ModelManifest> = parse_json("model manifest", &models_text, errors);
    let native_downloads = parse_module_downloads(&module_text, "native_runtime", true, errors);
    let model_downloads = parse_module_downloads(&module_text, "model_file", false, errors);

    if let Some(ort) = &ort {
        validate_ort_manifest(inventory, ort, &native_downloads, errors);
    }
    if let Some(models) = &models {
        validate_model_manifest(inventory, models, &model_downloads, errors);
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_module_downloads(
    module: &str,
    rule_name: &str,
    uses_integrity: bool,
    errors: &mut Vec<String>,
) -> BTreeMap<String, Download> {
    let mut downloads = BTreeMap::new();
    let marker = format!("{rule_name}(");
    let lines: Vec<_> = module.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != marker {
            index += 1;
            continue;
        }
        let block_start = index + 1;
        let Some(relative_end) = lines[block_start..].iter().position(|line| line.trim() == ")")
        else {
            errors.push(format!("unterminated {rule_name} declaration in MODULE.bazel"));
            break;
        };
        let block_end = block_start + relative_end;
        let block = &lines[block_start..block_end];
        let name = module_attribute(block, "name", errors);
        let url = module_attribute(block, "urls", errors);
        let hash_field = if uses_integrity { "integrity" } else { "sha256" };
        let hash = module_attribute(block, hash_field, errors);
        if let (Some(name), Some(url), Some(hash)) = (name, url, hash) {
            let sha256 = if uses_integrity {
                decode_integrity(&hash).unwrap_or_else(|error| {
                    errors.push(format!("MODULE.bazel {name}: {error}"));
                    String::new()
                })
            } else {
                hash
            };
            if downloads.insert(name.clone(), Download { url, sha256 }).is_some() {
                errors.push(format!("duplicate MODULE.bazel download repository {name}"));
            }
        }
        index = block_end + 1;
    }
    downloads
}

fn module_attribute(block: &[&str], key: &str, errors: &mut Vec<String>) -> Option<String> {
    let prefix = format!("{key} = ");
    let values: Vec<_> =
        block.iter().filter_map(|line| line.trim().strip_prefix(&prefix)).collect();
    if values.len() != 1 {
        errors.push(format!("MODULE.bazel declaration must contain exactly one {key}"));
        return None;
    }
    let value = values[0].strip_suffix(',').unwrap_or(values[0]);
    let quoted = if key == "urls" {
        value.strip_prefix("[\"").and_then(|value| value.strip_suffix("\"]"))
    } else {
        value.strip_prefix('"').and_then(|value| value.strip_suffix('"'))
    };
    quoted.map(str::to_owned).or_else(|| {
        errors.push(format!("MODULE.bazel {key} must be one literal string"));
        None
    })
}

fn decode_integrity(integrity: &str) -> Result<String, String> {
    let encoded =
        integrity.strip_prefix("sha256-").ok_or_else(|| "integrity must use sha256".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid integrity base64: {error}"))?;
    if bytes.len() != 32 {
        return Err("integrity digest is not 32 bytes".to_owned());
    }
    let mut digest = String::with_capacity(64);
    for byte in bytes {
        write!(&mut digest, "{byte:02x}")
            .map_err(|error| format!("cannot format integrity digest: {error}"))?;
    }
    Ok(digest)
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
    downloads: &BTreeMap<String, Download>,
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
    if manifest.source != expected_source {
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
    let expected_repositories: BTreeSet<_> = expected_targets.values().copied().collect();
    let actual_repositories: BTreeSet<_> = downloads.keys().map(String::as_str).collect();
    if actual_repositories != expected_repositories {
        errors.push(
            "MODULE.bazel native runtime repositories do not match supported ONNX targets"
                .to_owned(),
        );
    }
    for (target, repository) in expected_targets {
        let (Some(asset), Some(download)) =
            (manifest.targets.get(target), downloads.get(repository))
        else {
            continue;
        };
        if !is_sha256(&asset.sha256) {
            errors.push(format!("ONNX Runtime target {target} lacks a valid SHA-256"));
        }
        let expected_url = format!(
            "https://github.com/microsoft/onnxruntime/releases/download/v{}/{}",
            manifest.version, asset.asset
        );
        if download.url != expected_url || download.sha256 != asset.sha256 {
            errors.push(format!(
                "ONNX Runtime target {target} URL/hash disagrees with MODULE.bazel repository {repository}"
            ));
        }
    }
}

fn model_repository(id: &str) -> Option<&'static str> {
    match id {
        "pp-ocrv6-tiny-detector-source" => Some("ppocrv6_tiny_detector_source"),
        "pp-ocrv6-tiny-recognizer-source" => Some("ppocrv6_tiny_recognizer_source"),
        _ => None,
    }
}

fn validate_model_manifest(
    inventory: &Inventory,
    manifest: &ModelManifest,
    downloads: &BTreeMap<String, Download>,
    errors: &mut Vec<String>,
) {
    let mut artifacts = BTreeMap::new();
    for bundle in &manifest.bundles {
        for artifact in &bundle.source_artifacts {
            if artifacts
                .insert(artifact.id.as_str(), (artifact, bundle.upstream_version.as_str()))
                .is_some()
            {
                errors.push(format!("duplicate model artifact {} across bundles", artifact.id));
            }
        }
    }
    for required in ["pp-ocrv6-tiny-detector-source", "pp-ocrv6-tiny-recognizer-source"] {
        if !artifacts.contains_key(required) {
            errors.push(format!("model manifest lacks required artifact {required}"));
        }
    }

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

    let mut expected_repositories = BTreeSet::new();
    for (id, (artifact, upstream_version)) in artifacts {
        let Some(component) = inventory_sources.get(id) else {
            continue;
        };
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
        if !is_sha256(&artifact.sha256) {
            errors.push(format!("model manifest entry {id} lacks a valid SHA-256"));
        }
        let Some(repository) = model_repository(id) else {
            errors.push(format!("managed model artifact {id} has no Bazel repository mapping"));
            continue;
        };
        expected_repositories.insert(repository);
        match downloads.get(repository) {
            Some(download)
                if download.url == artifact.url && download.sha256 == artifact.sha256 => {}
            Some(_) => errors.push(format!(
                "model artifact {id} URL/hash disagrees with MODULE.bazel repository {repository}"
            )),
            None => errors
                .push(format!("model artifact {id} lacks MODULE.bazel repository {repository}")),
        }
    }
    let actual_repositories: BTreeSet<_> = downloads.keys().map(String::as_str).collect();
    if actual_repositories != expected_repositories {
        errors.push(
            "MODULE.bazel model repositories do not match managed model artifacts".to_owned(),
        );
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

    fn artifact(id: &str, marker: char) -> ModelArtifact {
        ModelArtifact {
            id: id.to_owned(),
            url: format!("https://example.invalid/{id}.tar"),
            sha256: marker.to_string().repeat(64),
            license: "Apache-2.0".to_owned(),
        }
    }

    fn model_fixture() -> (ModelManifest, Inventory, BTreeMap<String, Download>) {
        let detector = artifact("pp-ocrv6-tiny-detector-source", 'a');
        let recognizer = artifact("pp-ocrv6-tiny-recognizer-source", 'b');
        let version = "test model";
        let inventory = Inventory {
            schema_version: 1,
            components: vec![
                model_component(&detector.id, &detector, version),
                model_component(&recognizer.id, &recognizer, version),
            ],
        };
        let downloads = BTreeMap::from([
            (
                "ppocrv6_tiny_detector_source".to_owned(),
                Download { url: detector.url.clone(), sha256: detector.sha256.clone() },
            ),
            (
                "ppocrv6_tiny_recognizer_source".to_owned(),
                Download { url: recognizer.url.clone(), sha256: recognizer.sha256.clone() },
            ),
        ]);
        let manifest = ModelManifest {
            bundles: vec![ModelBundle {
                upstream_version: version.to_owned(),
                source_artifacts: vec![detector, recognizer],
            }],
        };
        (manifest, inventory, downloads)
    }

    fn ort_fixture() -> (OrtManifest, Inventory, BTreeMap<String, Download>) {
        let version = "1.2.3";
        let target_repositories = [
            ("aarch64-apple-darwin", "onnxruntime_macos_arm64", "mac.tgz", 'a'),
            ("aarch64-unknown-linux-gnu", "onnxruntime_linux_arm64", "arm.tgz", 'b'),
            ("x86_64-pc-windows-msvc", "onnxruntime_windows_x86_64", "windows.zip", 'c'),
            ("x86_64-unknown-linux-gnu", "onnxruntime_linux_x86_64", "linux.tgz", 'd'),
        ];
        let mut targets = BTreeMap::new();
        let mut downloads = BTreeMap::new();
        for (target, repository, asset, marker) in target_repositories {
            let sha256 = marker.to_string().repeat(64);
            targets.insert(
                target.to_owned(),
                OrtTarget { asset: asset.to_owned(), sha256: sha256.clone() },
            );
            downloads.insert(
                repository.to_owned(),
                Download {
                    url: format!(
                        "https://github.com/microsoft/onnxruntime/releases/download/v{version}/{asset}"
                    ),
                    sha256,
                },
            );
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
        (manifest, inventory, downloads)
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
    fn commented_module_declaration_is_not_treated_as_a_download() {
        let module = r#"# native_runtime(
#     name = "onnxruntime_macos_arm64",
#     integrity = "sha256-YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=",
#     urls = ["https://example.invalid/runtime.tgz"],
# )
"#;
        let mut errors = Vec::new();
        let downloads = parse_module_downloads(module, "native_runtime", true, &mut errors);
        assert!(downloads.is_empty());
        assert!(errors.is_empty());
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
            &BTreeMap::new(),
            &mut errors,
        );
        assert!(errors.iter().any(|error| error.contains("onnxruntime-cpu is missing")));
    }

    #[test]
    fn onnx_platform_url_and_hash_drift_is_rejected() {
        let (mut manifest, inventory, mut downloads) = ort_fixture();
        downloads.get_mut("onnxruntime_macos_arm64").unwrap().url.push_str(".wrong");
        manifest.targets.remove("x86_64-pc-windows-msvc");
        let mut errors = Vec::new();
        validate_ort_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("exact four supported target keys")));
        assert!(errors.iter().any(|error| {
            error.contains("aarch64-apple-darwin") && error.contains("disagrees with MODULE.bazel")
        }));
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
    fn artifact_in_later_bundle_is_not_ignored() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(ModelBundle {
            upstream_version: "later".to_owned(),
            source_artifacts: vec![artifact("later-bundle-artifact", 'c')],
        });
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains("later-bundle-artifact has no inventory"))
        );
    }

    #[test]
    fn duplicate_model_artifact_across_bundles_is_rejected() {
        let (mut manifest, inventory, downloads) = model_fixture();
        manifest.bundles.push(ModelBundle {
            upstream_version: "duplicate".to_owned(),
            source_artifacts: vec![artifact("pp-ocrv6-tiny-detector-source", 'd')],
        });
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("duplicate model artifact")));
    }

    #[test]
    fn orphan_model_inventory_component_is_rejected() {
        let (manifest, mut inventory, downloads) = model_fixture();
        let orphan = artifact("orphan", 'e');
        inventory.components.push(model_component(&orphan.id, &orphan, "orphan"));
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| error.contains("orphan has no manifest artifact")));
    }

    #[test]
    fn model_module_url_and_hash_drift_is_rejected() {
        let (manifest, inventory, mut downloads) = model_fixture();
        downloads.get_mut("ppocrv6_tiny_detector_source").unwrap().sha256 = "f".repeat(64);
        let mut errors = Vec::new();
        validate_model_manifest(&inventory, &manifest, &downloads, &mut errors);
        assert!(errors.iter().any(|error| {
            error.contains("detector-source URL/hash disagrees with MODULE.bazel")
        }));
    }
}
