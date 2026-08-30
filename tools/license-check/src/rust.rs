//! Cargo.lock-backed release component authority.

use crate::schema::{Component, IntegrityEvidence};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
struct CargoLock {
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[cfg(test)]
    #[serde(default)]
    dependencies: Vec<String>,
}

#[allow(clippy::too_many_lines)] // Parsing, closure selection, and authority joins stay in one fail-closed pass.
pub(crate) fn load(
    repository: &std::path::Path,
    lock_text: &str,
    approvals: &str,
    normal_runtime: &str,
    runtime_root: &str,
    errors: &mut Vec<String>,
) -> Vec<Component> {
    load_selected_roots(
        repository,
        lock_text,
        approvals,
        normal_runtime,
        &[runtime_root],
        runtime_root == "into-markdown-cli",
        errors,
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn load_for_roots(
    repository: &std::path::Path,
    lock_text: &str,
    approvals: &str,
    normal_runtime: &str,
    runtime_roots: &[&str],
    errors: &mut Vec<String>,
) -> Vec<Component> {
    load_selected_roots(
        repository,
        lock_text,
        approvals,
        normal_runtime,
        runtime_roots,
        runtime_roots.contains(&"into-markdown-cli"),
        errors,
    )
}

#[allow(clippy::too_many_lines)]
fn load_selected_roots(
    repository: &std::path::Path,
    lock_text: &str,
    approvals: &str,
    normal_runtime: &str,
    runtime_roots: &[&str],
    include_reviewed_core: bool,
    errors: &mut Vec<String>,
) -> Vec<Component> {
    let lock: CargoLock = match toml::from_str(lock_text) {
        Ok(lock) => lock,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock release authority: {error}"));
            return Vec::new();
        }
    };
    let approved = crate::parse_approvals(approvals, errors);
    let registry_packages = lock
        .package
        .iter()
        .filter(|package| {
            package.source.as_deref()
                == Some("registry+https://github.com/rust-lang/crates.io-index")
        })
        .map(|package| (package.name.clone(), package.version.clone()))
        .collect();
    let reviewed_core = crate::cargo_runtime::packages(
        repository,
        lock_text,
        &registry_packages,
        normal_runtime,
        errors,
    );
    let mut required = if include_reviewed_core {
        reviewed_core
    } else {
        crate::cargo_runtime::RuntimeClosure { registry: BTreeSet::new(), local: BTreeMap::new() }
    };
    for runtime_root in runtime_roots {
        if *runtime_root == "into-markdown-cli" {
            continue;
        }
        let additional = crate::cargo_runtime::packages_for_root(repository, runtime_root, errors);
        required.registry.extend(additional.registry);
        required.local.extend(additional.local);
    }
    let locked: BTreeSet<_> = lock
        .package
        .iter()
        .map(|package| (package.name.clone(), package.version.clone()))
        .collect();
    let mut components = Vec::new();
    for package in lock.package {
        if package.source.as_deref()
            != Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            continue;
        }
        let key = (package.name, package.version);
        let Some(license) = approved.get(&key) else {
            errors.push(format!("unreviewed Cargo release component {}@{}", key.0, key.1));
            continue;
        };
        if package.checksum.as_deref().is_none_or(|hash| {
            hash.len() != 64
                || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            errors.push(format!("Cargo release component {}@{} lacks SHA-256", key.0, key.1));
        }
        components.push(Component {
            id: format!("cargo:{}@{}", key.0, key.1),
            kind: "rust-library".to_owned(),
            status: "reviewed".to_owned(),
            included_in_release: false,
            release_eligible: required.registry.contains(&key),
            manual_only: false,
            required_in_core: required.registry.contains(&key),
            version: Some(key.1.clone()),
            source: Some(format!("https://crates.io/crates/{}/{}", key.0, key.1)),
            license: Some(license.clone()),
            obligations: Some(
                "Preserve the concluded upstream license and notices; Cargo.lock fixes the exact crates.io checksum."
                    .to_owned(),
            ),
            integrity: package
                .checksum
                .map(|checksum| vec![IntegrityEvidence {
                    algorithm: "SHA-256".to_owned(),
                    digest: checksum,
                    subject: format!("crates.io archive {}@{}", key.0, key.1),
                    target: None,
                }])
                .unwrap_or_default(),
            authority: "Cargo.lock + third_party/licenses/rust-lock.tsv + third_party/licenses/cargo-normal-runtime.json".to_owned(),
        });
    }
    for (key, package) in required.local {
        let Some(license) = approved.get(&key) else {
            errors.push(format!("unreviewed local Cargo release component {}@{}", key.0, key.1));
            continue;
        };
        let is_whisper_sys = key == ("whisper-rs-sys".to_owned(), "0.15.0".to_owned());
        let source_authority = is_whisper_sys
            .then(|| crate::release_authority::whisper_rs_sys(repository, errors))
            .flatten();
        let license_matches = if is_whisper_sys {
            package.license == "Unlicense" && license == "Unlicense AND MIT"
        } else {
            *license == package.license
        };
        if !license_matches {
            errors.push(format!(
                "local Cargo release component {}@{} license differs: authority={license}, metadata={}",
                key.0, key.1, package.license
            ));
        }
        let integrity = source_authority.as_ref().map_or_else(Vec::new, |authority| {
            vec![
                IntegrityEvidence {
                    algorithm: "SHA-256".to_owned(),
                    digest: authority.crates_io_sha256.clone(),
                    subject: "crates.io archive whisper-rs-sys@0.15.0".to_owned(),
                    target: None,
                },
                IntegrityEvidence {
                    algorithm: "SHA-256".to_owned(),
                    digest: authority.sha256.clone(),
                    subject: "reviewed deterministic vendored source archive whisper-rs-sys@0.15.0"
                        .to_owned(),
                    target: None,
                },
            ]
        });
        components.push(Component {
            id: format!("cargo:{}@{}", key.0, key.1),
            kind: "rust-library".to_owned(),
            status: "reviewed".to_owned(),
            included_in_release: false,
            release_eligible: true,
            manual_only: false,
            required_in_core: true,
            version: Some(key.1.clone()),
            source: Some(if is_whisper_sys {
                "https://codeberg.org/tazz4843/whisper-rs/src/commit/7558e1b72f54f2f22a53589afb77e65681834c36/sys"
                    .to_owned()
            } else {
                "https://codeberg.org/tazz4843/whisper-rs".to_owned()
            }),
            license: Some(license.clone()),
            obligations: Some(
                "Preserve the concluded vendored license and reviewed source provenance."
                    .to_owned(),
            ),
            integrity,
            authority: if is_whisper_sys {
                "Cargo.lock + vendored Cargo.toml + third_party/licenses/rust-lock.tsv + third_party/licenses/cargo-normal-runtime.json + third_party/licenses/release-material-authority.json"
                    .to_owned()
            } else {
                "Cargo.lock + vendored Cargo.toml + third_party/licenses/rust-lock.tsv + third_party/licenses/cargo-normal-runtime.json"
                    .to_owned()
            },
        });
    }
    for stale in approved.keys().filter(|key| !locked.contains(*key)) {
        errors.push(format!("stale Rust release authority {}@{}", stale.0, stale.1));
    }
    components
}

pub(crate) fn validate_bazel_bridge(repository: &std::path::Path, errors: &mut Vec<String>) {
    validate_bazel_runtime_graph(repository, errors);
    let module = std::fs::read_to_string(repository.join("MODULE.bazel")).unwrap_or_default();
    if !module.contains("crate.from_cargo(")
        || !module.contains("cargo_lockfile = \"//:Cargo.lock\"")
        || !module.contains("manifests = [\"//:Cargo.toml\"]")
    {
        errors
            .push("Bazel crate universe is not bound to the workspace Cargo authority".to_owned());
    }
}

fn validate_bazel_runtime_graph(repository: &std::path::Path, errors: &mut Vec<String>) {
    #[cfg(test)]
    let lock = std::fs::read_to_string(repository.join("Cargo.lock")).unwrap_or_default();
    #[cfg(test)]
    let parsed: CargoLock = match toml::from_str(&lock) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock Bazel authority: {error}"));
            return;
        }
    };
    #[cfg(not(test))]
    let cargo_packages = crate::cargo_runtime::workspace_normal_packages(repository, errors);
    #[cfg(test)]
    let cargo_packages = if repository.join("Cargo.toml").is_file() {
        crate::cargo_runtime::workspace_normal_packages(repository, errors)
    } else {
        workspace_runtime_closure(&parsed.package, "into-markdown-cli", errors)
    };
    let mut bazel_packages = BTreeSet::new();
    let mut pending = vec!["//apps/cli:into-md".to_owned()];
    let mut visited = BTreeSet::new();
    while let Some(label) = pending.pop() {
        if !visited.insert(label.clone()) {
            continue;
        }
        let Some((package, target)) =
            label.strip_prefix("//").and_then(|value| value.split_once(':'))
        else {
            errors.push(format!("invalid internal Bazel runtime label {label}"));
            continue;
        };
        let path = repository.join(package).join("BUILD.bazel");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            errors.push(format!("cannot read {}: {error}", path.display()));
            String::new()
        });
        let text = strip_comments(&text);
        let rule_kind = if package == "apps/cli" { "rust_binary" } else { "rust_library" };
        let Some(body) = named_rule_body(&text, rule_kind, target) else {
            errors.push(format!("missing {rule_kind} {label}"));
            continue;
        };
        if package == "apps/cli" {
            if !attribute(body, "deps")
                .is_some_and(|value| contains_unquoted(value, "all_crate_deps(normal = True)"))
                || attribute(body, "compile_data").map(str::trim)
                    != Some("[\"//web/console:checked_assets\"]")
            {
                errors.push(
                    "Bazel into-md target is not bound to Cargo runtime deps and console assets"
                        .to_owned(),
                );
            }
        } else if let Some(name) = cargo_package_name(&repository.join(package).join("Cargo.toml"))
        {
            bazel_packages.insert(name);
        } else {
            errors.push(format!("Bazel runtime package {package} lacks a Cargo package name"));
        }
        let deps = attribute(body, "deps").unwrap_or_default();
        validate_direct_external_deps(
            &repository.join(package).join("Cargo.toml"),
            &label,
            deps,
            errors,
        );
        for dependency in quoted_values(deps) {
            if let Some(rest) = dependency.strip_prefix("//crates/") {
                let normalized = if rest.contains(':') {
                    format!("//crates/{rest}")
                } else {
                    let name = rest.rsplit('/').next().unwrap_or(rest).replace('-', "_");
                    format!("//crates/{rest}:{name}")
                };
                pending.push(normalized);
            } else if let Some(rest) = dependency.strip_prefix("//") {
                let package = rest.split_once(':').map_or(rest, |(package, _)| package);
                if repository.join(package).join("Cargo.toml").is_file() {
                    pending.push(dependency.to_owned());
                }
            }
        }
    }
    let cargo_packages: BTreeSet<_> =
        cargo_packages.into_iter().filter(|name| name != "into-markdown-cli").collect();
    if cargo_packages != bazel_packages {
        errors.push(format!(
            "Cargo/Bazel workspace runtime closure differs: Cargo={cargo_packages:?}, Bazel={bazel_packages:?}"
        ));
    }
}

#[cfg(test)]
fn workspace_runtime_closure(
    packages: &[LockedPackage],
    root: &str,
    errors: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, package) in packages.iter().enumerate() {
        by_name.entry(&package.name).or_default().push(index);
    }
    let Some(root_index) =
        by_name.get(root).and_then(|values| (values.len() == 1).then_some(values[0]))
    else {
        return BTreeSet::new();
    };
    let mut seen = BTreeSet::new();
    let mut pending = vec![root_index];
    while let Some(index) = pending.pop() {
        if !seen.insert(index) {
            continue;
        }
        for dependency in &packages[index].dependencies {
            let name = dependency.split_whitespace().next().unwrap_or_default();
            if let Some(candidates) = by_name.get(name)
                && candidates.len() == 1
            {
                pending.push(candidates[0]);
            } else {
                errors.push(format!(
                    "Cargo.lock workspace dependency {dependency:?} is ambiguous or missing"
                ));
            }
        }
    }
    seen.into_iter()
        .filter(|&index| packages[index].source.is_none())
        .map(|index| packages[index].name.clone())
        .collect()
}

fn validate_direct_external_deps(
    manifest_path: &std::path::Path,
    label: &str,
    bazel_deps: &str,
    errors: &mut Vec<String>,
) {
    let manifest: toml::Value = match std::fs::read_to_string(manifest_path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
    {
        Some(value) => value,
        None => return,
    };
    let mut expected: BTreeSet<_> = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|table| table.iter())
        .filter(|(_, value)| value.as_table().is_none_or(|table| !table.contains_key("path")))
        .map(|(name, _)| name.as_str())
        .collect();
    for dependencies in manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values())
        .filter_map(|target| target.get("dependencies"))
        .filter_map(toml::Value::as_table)
    {
        expected.extend(
            dependencies
                .iter()
                .filter(|(_, value)| {
                    value.as_table().is_none_or(|table| !table.contains_key("path"))
                })
                .map(|(name, _)| name.as_str()),
        );
    }
    let explicit: BTreeSet<_> = quoted_values(bazel_deps)
        .into_iter()
        .filter_map(|value| value.strip_prefix("@crates//:"))
        .collect();
    let uses_macro = contains_unquoted(bazel_deps, "all_crate_deps(normal = True)");
    if (uses_macro && !explicit.is_subset(&expected)) || (!uses_macro && explicit != expected) {
        errors.push(format!(
            "Cargo/Bazel direct external runtime dependencies differ for {label}: Cargo={expected:?}, Bazel={explicit:?}, macro={uses_macro}"
        ));
    }
}

fn named_rule_body<'a>(text: &'a str, kind: &str, name: &str) -> Option<&'a str> {
    let needle = format!("{kind}(");
    let mut offset = 0;
    while let Some(found) = text[offset..].find(&needle) {
        let start = offset + found + needle.len();
        let end = balanced_end(text, start)?;
        let body = &text[start..end];
        if attribute(body, "name").map(str::trim) == Some(format!("\"{name}\"").as_str()) {
            return Some(body);
        }
        offset = end + 1;
    }
    None
}

fn balanced_end(text: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut quoted = false;
    let mut escaped = false;
    for (relative, byte) in text.as_bytes()[start..].iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            depth -= 1;
            if depth == 0 {
                return Some(start + relative);
            }
        }
    }
    None
}

fn attribute<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name} =");
    let found = body.match_indices(&marker).find_map(|(index, _)| {
        let boundary = index
            .checked_sub(1)
            .and_then(|previous| body.as_bytes().get(previous))
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
        boundary.then_some(index)
    })?;
    let start = found + marker.len();
    let end = balanced_end_attribute(body, start);
    Some(body[start..end].trim())
}

fn strip_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut quoted = false;
        let mut escaped = false;
        let mut comment = false;
        for character in line.chars() {
            if comment {
                if character == '\n' {
                    output.push(character);
                }
                continue;
            }
            if quoted {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quoted = false;
                }
            } else if character == '"' {
                quoted = true;
            } else if character == '#' {
                comment = true;
                continue;
            }
            output.push(character);
        }
    }
    output
}

fn contains_unquoted(text: &str, needle: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        } else if text[index..].starts_with(needle) {
            return true;
        }
    }
    false
}

fn balanced_end_attribute(text: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut quoted = false;
    for (relative, byte) in text.as_bytes()[start..].iter().copied().enumerate() {
        if byte == b'"' {
            quoted = !quoted;
        }
        if !quoted {
            if matches!(byte, b'(' | b'[' | b'{') {
                depth += 1;
            } else if matches!(byte, b')' | b']' | b'}') {
                depth = depth.saturating_sub(1);
            } else if byte == b',' && depth == 0 {
                return start + relative;
            }
        }
    }
    text.len()
}

fn quoted_values(value: &str) -> Vec<&str> {
    value.split('"').skip(1).step_by(2).collect()
}

fn cargo_package_name(path: &std::path::Path) -> Option<String> {
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value.get("package")?.get("name")?.as_str().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        CargoLock, attribute, contains_unquoted, load, named_rule_body, strip_comments,
        validate_bazel_runtime_graph,
    };
    use std::fs;

    #[test]
    fn bridge_tokens_must_belong_to_named_runtime_binary() {
        let build = r#"
rust_binary(name = "into-md", deps = ["//crates/api:into_markdown"], compile_data = [])
rust_test(name = "decoy", deps = all_crate_deps(normal = True), compile_data = ["//web/console:checked_assets"])
"#;
        let body = named_rule_body(build, "rust_binary", "into-md").unwrap();
        assert!(!attribute(body, "deps").unwrap().contains("all_crate_deps"));
        assert_eq!(attribute(body, "compile_data"), Some("[]"));
        let commented = strip_comments(&format!(
            "# rust_binary(name = \"into-md\", deps = all_crate_deps(normal = True))\n{build}"
        ));
        let body = named_rule_body(&commented, "rust_binary", "into-md").unwrap();
        assert!(!attribute(body, "deps").unwrap().contains("all_crate_deps"));
        assert_eq!(
            attribute("crate_name = \"wrong\", name = \"right\"", "name"),
            Some("\"right\"")
        );
        assert!(!contains_unquoted(
            "[\"all_crate_deps(normal = True)\"]",
            "all_crate_deps(normal = True)"
        ));
    }

    #[test]
    fn recursive_internal_bazel_runtime_omission_is_rejected() {
        let nonce =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root =
            std::env::temp_dir().join(format!("license-bazel-{}-{nonce}", std::process::id()));
        for path in ["apps/cli", "crates/a", "crates/b"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(
            root.join("Cargo.lock"),
            r#"version = 4
[[package]]
name = "into-markdown-cli"
version = "0.0.0"
dependencies = ["a"]
[[package]]
name = "a"
version = "0.0.0"
dependencies = ["b", "serde 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"]
[[package]]
name = "b"
version = "0.0.0"
[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        )
        .unwrap();
        fs::write(
            root.join("apps/cli/BUILD.bazel"),
            r#"rust_binary(name = "into-md", deps = ["//crates/a"] + all_crate_deps(normal = True), compile_data = ["//web/console:checked_assets"])"#,
        )
        .unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.0.0\"\n[dependencies]\nb = { path = \"../b\" }\nserde = \"1\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/a/BUILD.bazel"),
            r#"rust_library(name = "a", deps = ["//crates/b", "@crates//:serde"])"#,
        )
        .unwrap();
        fs::write(
            root.join("crates/b/BUILD.bazel"),
            r#"rust_library(name = "b", deps = all_crate_deps(normal = True))"#,
        )
        .unwrap();
        let mut errors = Vec::new();
        validate_bazel_runtime_graph(&root, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        fs::write(
            root.join("crates/a/BUILD.bazel"),
            r#"rust_library(name = "a", deps = ["//crates/b"])"#,
        )
        .unwrap();
        validate_bazel_runtime_graph(&root, &mut errors);
        assert!(errors.iter().any(|error| error.contains("direct external")));
        errors.clear();
        fs::write(
            root.join("crates/a/BUILD.bazel"),
            r#"rust_library(name = "a", deps = ["@crates//:serde"])"#,
        )
        .unwrap();
        validate_bazel_runtime_graph(&root, &mut errors);
        assert!(errors.iter().any(|error| error.contains("runtime closure differs")));
        fs::remove_dir_all(root).unwrap();
    }

    fn normal_authority_fixture() -> (std::path::PathBuf, String, CargoLock, String) {
        let root = crate::repository_root().unwrap();
        let lock_text = fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let lock: CargoLock = toml::from_str(&lock_text).unwrap();
        let authority_text =
            fs::read_to_string(root.join("third_party/licenses/cargo-normal-runtime.json"))
                .unwrap();
        (root, lock_text, lock, authority_text)
    }

    fn normal_authority_errors(
        root: &std::path::Path,
        lock_text: &str,
        lock: &CargoLock,
        authority: &serde_json::Value,
    ) -> Vec<String> {
        let locked = lock
            .package
            .iter()
            .filter(|package| package.source.is_some())
            .map(|package| (package.name.clone(), package.version.clone()))
            .collect();
        let mut errors = Vec::new();
        crate::cargo_runtime::packages(
            root,
            lock_text,
            &locked,
            &serde_json::to_string(authority).unwrap(),
            &mut errors,
        );
        errors
    }

    #[test]
    fn normal_runtime_authority_matches_locked_metadata() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn normal_runtime_authority_rejects_overlap_and_omission() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        let normal = authority["normal_registry_packages"].as_array_mut().unwrap();
        assert!(!normal.iter().any(|value| value == "cc@1.4.2"));
        assert!(normal.iter().any(|value| value == "serde@1.0.229"));

        normal.push(serde_json::Value::String("cc@1.4.2".into()));
        normal.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("exactly partition")));

        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        authority["normal_registry_packages"]
            .as_array_mut()
            .unwrap()
            .retain(|value| value != "serde@1.0.229");
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("exactly partition")));
    }

    #[test]
    fn normal_runtime_authority_rejects_kind_reclassification() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        for (list, remove, insert) in [
            ("normal_registry_packages", "serde@1.0.229", "cc@1.4.2"),
            ("non_normal_registry_packages", "cc@1.4.2", "serde@1.0.229"),
        ] {
            let values = authority[list].as_array_mut().unwrap();
            values.retain(|value| value != remove);
            values.push(serde_json::Value::String(insert.into()));
            values.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        }
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("metadata normal")));
    }

    #[test]
    fn normal_runtime_authority_rejects_manifest_set_drift() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        authority["workspace_manifest_sha256"]
            .as_object_mut()
            .unwrap()
            .remove("apps/cli/Cargo.toml");
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("exactly bind")));

        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        authority["workspace_manifest_sha256"]["not-a-member/Cargo.toml"] =
            serde_json::Value::String("0".repeat(64));
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("exactly bind")));
    }

    #[test]
    fn normal_runtime_authority_rejects_manifest_hash_drift() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        authority["workspace_manifest_sha256"]["apps/cli/Cargo.toml"] =
            serde_json::Value::String("0".repeat(64));
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("apps/cli/Cargo.toml")));
    }

    #[test]
    fn optional_local_model_runtime_is_excluded_from_core_authority() {
        let (root, lock_text, lock, authority_text) = normal_authority_fixture();
        let mut authority: serde_json::Value = serde_json::from_str(&authority_text).unwrap();
        authority["local_runtime_packages"] = serde_json::json!(["whisper-rs@0.16.0"]);
        let errors = normal_authority_errors(&root, &lock_text, &lock, &authority);
        assert!(errors.iter().any(|error| error.contains("local runtime authority")));

        let approvals = fs::read_to_string(root.join("third_party/licenses/rust-lock.tsv"))
            .unwrap()
            .replace("whisper-rs\t0.16.0\tUnlicense", "whisper-rs\t0.16.0\tApache-2.0");
        let mut errors = Vec::new();
        let components =
            load(&root, &lock_text, &approvals, &authority_text, "into-markdown-cli", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!components.iter().any(|component| component.id == "cargo:whisper-rs@0.16.0"));
    }

    fn release_authority_errors(lock_text: &str, approvals: &str) -> Vec<String> {
        let root = crate::repository_root().unwrap();
        let normal_runtime =
            fs::read_to_string(root.join("third_party/licenses/cargo-normal-runtime.json"))
                .unwrap();
        let mut errors = Vec::new();
        load(&root, lock_text, approvals, &normal_runtime, "into-markdown-cli", &mut errors);
        errors
    }

    #[test]
    fn release_authority_rejects_unknown_locked_component() {
        let root = crate::repository_root().unwrap();
        let mut lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
        lock.push_str(
            r#"
[[package]]
name = "authority-attack"
version = "9.9.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        );
        let approvals =
            fs::read_to_string(root.join("third_party/licenses/rust-lock.tsv")).unwrap();
        let errors = release_authority_errors(&lock, &approvals);
        assert!(
            errors
                .iter()
                .any(|error| error == "unreviewed Cargo release component authority-attack@9.9.9"),
            "{errors:?}"
        );
    }

    #[test]
    fn release_authority_rejects_locked_version_change() {
        let root = crate::repository_root().unwrap();
        let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap().replacen(
            "name = \"sysinfo\"\nversion = \"0.37.2\"",
            "name = \"sysinfo\"\nversion = \"0.37.3\"",
            1,
        );
        assert!(lock.contains("name = \"sysinfo\"\nversion = \"0.37.3\""));
        let approvals =
            fs::read_to_string(root.join("third_party/licenses/rust-lock.tsv")).unwrap();
        let errors = release_authority_errors(&lock, &approvals);
        assert!(
            errors.iter().any(|error| error == "unreviewed Cargo release component sysinfo@0.37.3"),
            "{errors:?}"
        );
    }

    #[test]
    fn release_authority_rejects_wildcard_approval() {
        let root = crate::repository_root().unwrap();
        let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let approvals = fs::read_to_string(root.join("third_party/licenses/rust-lock.tsv"))
            .unwrap()
            .replace("sysinfo\t0.37.2\tMIT", "sysinfo\t*\tMIT");
        let errors = release_authority_errors(&lock, &approvals);
        assert!(errors.iter().any(|error| error.contains("must not contain wildcards")));
        assert!(
            errors.iter().any(|error| error == "unreviewed Cargo release component sysinfo@0.37.2"),
            "{errors:?}"
        );
    }
}
