//! Offline validation for the repository's license policy and inventories.

use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

#[derive(Debug, Deserialize)]
struct Policy {
    schema_version: u64,
    allowed: BTreeSet<String>,
    denied: BTreeSet<String>,
    require_known_for_release: bool,
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

    let policy: Option<Policy> = parse_json("policy.json", &policy_text, &mut errors);
    let inventory: Option<Inventory> = parse_json("inventory.json", &inventory_text, &mut errors);

    if let Some(policy) = &policy {
        validate_policy(policy, &mut errors);
        validate_rust_lock(&lock_text, &approvals_text, policy, &mut errors);
    }
    if let (Some(policy), Some(inventory)) = (&policy, &inventory) {
        validate_inventory(inventory, policy, release, &mut errors);
        validate_existing_manifests(root, inventory, &mut errors);
    }
    validate_workspace_metadata(root, &mut errors);

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

fn validate_rust_lock(lock: &str, approvals: &str, policy: &Policy, errors: &mut Vec<String>) {
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
        if package.get("source").is_none() {
            continue;
        }
        let Some(name) = package.get("name").and_then(TomlValue::as_str) else {
            errors.push("Cargo.lock package lacks name".to_owned());
            continue;
        };
        let Some(version) = package.get("version").and_then(TomlValue::as_str) else {
            errors.push(format!("Cargo.lock package {name} lacks version"));
            continue;
        };
        let source = package.get("source").and_then(TomlValue::as_str);
        if source != Some("registry+https://github.com/rust-lang/crates.io-index") {
            errors
                .push(format!("Rust dependency {name}@{version} has unreviewed source {source:?}"));
        }
        let checksum = package.get("checksum").and_then(TomlValue::as_str);
        if checksum.is_none_or(|value| !is_sha256(value)) {
            errors.push(format!("Rust dependency {name}@{version} lacks a valid SHA-256"));
        }
        locked.insert((name.to_owned(), version.to_owned()));
    }

    for key in locked.difference(&approved.keys().cloned().collect()) {
        errors.push(format!("unreviewed Rust dependency {}@{}", key.0, key.1));
    }
    for key in approved.keys().collect::<BTreeSet<_>>().difference(&locked.iter().collect()) {
        errors.push(format!("stale Rust approval {}@{}", key.0, key.1));
    }
    for ((name, version), license) in approved {
        if !policy.allowed.contains(&license) {
            errors.push(format!(
                "Rust dependency {name}@{version} has non-allowed conclusion {license}"
            ));
        }
        if policy.denied.contains(&license) {
            errors
                .push(format!("Rust dependency {name}@{version} has denied conclusion {license}"));
        }
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
                if let Some(license) = &component.license
                    && (!policy.allowed.contains(license) || policy.denied.contains(license))
                {
                    errors.push(format!(
                        "component {} has non-allowed license {license}",
                        component.id
                    ));
                }
            }
            "planned" => {}
            other => errors.push(format!("component {} has unknown status {other}", component.id)),
        }
        if release
            && component.included_in_release
            && policy.require_known_for_release
            && component.status != "reviewed"
        {
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
    ] {
        if !ids.contains(required) {
            errors.push(format!("future component placeholder {required} is missing"));
        }
    }
}

fn validate_existing_manifests(root: &Path, inventory: &Inventory, errors: &mut Vec<String>) {
    let ort_text = read(&root.join("third_party/onnxruntime/manifest.json"), errors);
    let models_text = read(&root.join("models/manifest.json"), errors);
    let ort: Option<JsonValue> = parse_json("ONNX Runtime manifest", &ort_text, errors);
    let models: Option<JsonValue> = parse_json("model manifest", &models_text, errors);

    let find = |id: &str| inventory.components.iter().find(|component| component.id == id);
    if let (Some(ort), Some(component)) = (ort, find("onnxruntime-cpu")) {
        compare_json_field(&component.id, "version", component.version.as_deref(), &ort, errors);
        compare_json_field(&component.id, "source", component.source.as_deref(), &ort, errors);
        compare_json_field(&component.id, "license", component.license.as_deref(), &ort, errors);
        let targets = ort.get("targets").and_then(JsonValue::as_object);
        if targets.is_none_or(|targets| {
            targets.len() != 4
                || targets.values().any(|target| {
                    target
                        .get("sha256")
                        .and_then(JsonValue::as_str)
                        .is_none_or(|hash| !is_sha256(hash))
                })
        }) {
            errors.push("ONNX Runtime manifest must hash all four platform archives".to_owned());
        }
    }
    if let Some(models) = models {
        let artifacts = models.pointer("/bundles/0/source_artifacts").and_then(JsonValue::as_array);
        for id in ["pp-ocrv6-tiny-detector-source", "pp-ocrv6-tiny-recognizer-source"] {
            let Some(component) = find(id) else { continue };
            let artifact = artifacts.and_then(|items| {
                items.iter().find(|item| item.get("id").and_then(JsonValue::as_str) == Some(id))
            });
            let Some(artifact) = artifact else {
                errors.push(format!("model manifest lacks {id}"));
                continue;
            };
            compare_json_field(id, "source", component.source.as_deref(), artifact, errors);
            compare_json_field(id, "license", component.license.as_deref(), artifact, errors);
            if artifact
                .get("sha256")
                .and_then(JsonValue::as_str)
                .is_none_or(|hash| !is_sha256(hash))
            {
                errors.push(format!("model manifest entry {id} lacks a valid SHA-256"));
            }
        }
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn compare_json_field(
    id: &str,
    field: &str,
    expected: Option<&str>,
    value: &JsonValue,
    errors: &mut Vec<String>,
) {
    let manifest_field =
        if field == "source" && value.get("url").is_some() { "url" } else { field };
    if value.get(manifest_field).and_then(JsonValue::as_str) != expected {
        errors.push(format!("component {id} {field} disagrees with its source manifest"));
    }
}

fn validate_workspace_metadata(root: &Path, errors: &mut Vec<String>) {
    let root_text = read(&root.join("Cargo.toml"), errors);
    let manifest: TomlValue = match toml::from_str(&root_text) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid workspace Cargo.toml: {error}"));
            return;
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
            Ok(value)
                if value
                    .get("package")
                    .and_then(|package| package.get("license"))
                    .and_then(|license| license.get("workspace"))
                    .and_then(TomlValue::as_bool)
                    == Some(true) => {}
            Ok(_) => errors.push(format!("{member}/Cargo.toml must inherit workspace license")),
            Err(error) => errors.push(format!("invalid {member}/Cargo.toml: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_rejects_planned_included_component() {
        let policy = Policy {
            schema_version: 1,
            allowed: BTreeSet::from(["MIT".to_owned()]),
            denied: BTreeSet::from(["GPL-3.0".to_owned()]),
            require_known_for_release: true,
        };
        let mut components = [
            "pdfium",
            "ffmpeg",
            "libreoffice",
            "wasmtime",
            "generated-onnx-models",
            "distribution-fonts",
        ]
        .map(|id| Component {
            id: id.to_owned(),
            kind: "placeholder".to_owned(),
            status: "planned".to_owned(),
            included_in_release: false,
            version: None,
            source: None,
            license: None,
            obligations: None,
        });
        components[1].included_in_release = true;
        let inventory = Inventory { schema_version: 1, components: components.into() };
        let mut errors = Vec::new();
        validate_inventory(&inventory, &policy, true, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains("ffmpeg") && error.contains("not reviewed"))
        );
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
        let policy = Policy {
            schema_version: 1,
            allowed: BTreeSet::from(["MIT".to_owned()]),
            denied: BTreeSet::from(["GPL-3.0-only".to_owned()]),
            require_known_for_release: true,
        };
        let mut errors = Vec::new();
        validate_rust_lock(lock, "old\t1.0.0\tMIT\n", &policy, &mut errors);
        assert!(errors.iter().any(|error| error.contains("unreviewed source")));
        assert!(errors.iter().any(|error| error.contains("unreviewed Rust dependency new@1.0.0")));
        assert!(errors.iter().any(|error| error.contains("stale Rust approval old@1.0.0")));
    }
}
