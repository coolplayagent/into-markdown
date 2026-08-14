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
    #[serde(default)]
    dependencies: Vec<String>,
}

pub(crate) fn load(lock: &str, approvals: &str, errors: &mut Vec<String>) -> Vec<Component> {
    let lock: CargoLock = match toml::from_str(lock) {
        Ok(lock) => lock,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock release authority: {error}"));
            return Vec::new();
        }
    };
    let mut approved = BTreeMap::new();
    for (line_number, line) in approvals.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 3 {
            errors.push(format!("invalid Rust approval line {}", line_number + 1));
            continue;
        }
        let key = (fields[0].to_owned(), fields[1].to_owned());
        if approved.insert(key.clone(), fields[2].to_owned()).is_some() {
            errors.push(format!("duplicate Rust release authority {}@{}", key.0, key.1));
        }
    }
    let required = runtime_closure(&lock.package, "into-markdown-cli", errors);
    let mut locked = BTreeSet::new();
    let mut components = Vec::new();
    for package in lock.package {
        if package.source.as_deref()
            != Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            continue;
        }
        let key = (package.name, package.version);
        locked.insert(key.clone());
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
            release_eligible: true,
            manual_only: false,
            required_in_core: required.contains(&key),
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
            authority: "Cargo.lock + third_party/licenses/rust-lock.tsv".to_owned(),
        });
    }
    for stale in approved.keys().filter(|key| !locked.contains(*key)) {
        errors.push(format!("stale Rust release authority {}@{}", stale.0, stale.1));
    }
    components
}

fn runtime_closure(
    packages: &[LockedPackage],
    root: &str,
    errors: &mut Vec<String>,
) -> BTreeSet<(String, String)> {
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, package) in packages.iter().enumerate() {
        by_name.entry(&package.name).or_default().push(index);
    }
    let Some(root_index) =
        by_name.get(root).and_then(|items| (items.len() == 1).then_some(items[0]))
    else {
        errors.push(format!("Cargo.lock must contain one {root} package"));
        return BTreeSet::new();
    };
    let mut seen = BTreeSet::new();
    let mut pending = vec![root_index];
    while let Some(index) = pending.pop() {
        if !seen.insert(index) {
            continue;
        }
        for dependency in &packages[index].dependencies {
            let mut fields = dependency.split_whitespace();
            let name = fields.next().unwrap_or_default();
            let version = fields
                .next()
                .filter(|value| value.as_bytes().first().is_some_and(u8::is_ascii_digit));
            let candidates = by_name.get(name).cloned().unwrap_or_default();
            let matches: Vec<_> = candidates
                .into_iter()
                .filter(|candidate| version.is_none_or(|v| packages[*candidate].version == v))
                .collect();
            if matches.len() == 1 {
                pending.push(matches[0]);
            } else {
                errors.push(format!(
                    "Cargo.lock dependency {dependency:?} from {}@{} is ambiguous or missing",
                    packages[index].name, packages[index].version
                ));
            }
        }
    }
    seen.into_iter()
        .filter_map(|index| {
            let package = &packages[index];
            package.source.as_deref().map(|_| (package.name.clone(), package.version.clone()))
        })
        .collect()
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
    let lock = std::fs::read_to_string(repository.join("Cargo.lock")).unwrap_or_default();
    let parsed: CargoLock = match toml::from_str(&lock) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock Bazel authority: {error}"));
            return;
        }
    };
    let cargo_packages = workspace_runtime_closure(&parsed.package, "into-markdown-cli", errors);
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
        let rule_kind = if package == "apps/cli" { "rust_binary" } else { "rust_library" };
        let Some(body) = named_rule_body(&text, rule_kind, target) else {
            errors.push(format!("missing {rule_kind} {label}"));
            continue;
        };
        if package == "apps/cli" {
            if !attribute(body, "deps")
                .is_some_and(|value| value.contains("all_crate_deps(normal = True)"))
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
        for dependency in quoted_values(attribute(body, "deps").unwrap_or_default()) {
            if let Some(rest) = dependency.strip_prefix("//crates/") {
                let normalized = if rest.contains(':') {
                    format!("//crates/{rest}")
                } else {
                    let name = rest.rsplit('/').next().unwrap_or(rest).replace('-', "_");
                    format!("//crates/{rest}:{name}")
                };
                pending.push(normalized);
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
            let mut fields = dependency.split_whitespace();
            let name = fields.next().unwrap_or_default();
            let version = fields
                .next()
                .filter(|value| value.as_bytes().first().is_some_and(u8::is_ascii_digit));
            let matches: Vec<_> = by_name
                .get(name)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| {
                    version.is_none_or(|value| packages[*candidate].version == value)
                })
                .collect();
            if matches.len() == 1 {
                pending.push(matches[0]);
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
    let start = body.find(&marker)? + marker.len();
    let end = balanced_end_attribute(body, start);
    Some(body[start..end].trim())
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
    use super::{attribute, named_rule_body, validate_bazel_runtime_graph};
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
dependencies = ["b"]
[[package]]
name = "b"
version = "0.0.0"
"#,
        )
        .unwrap();
        fs::write(
            root.join("apps/cli/BUILD.bazel"),
            r#"rust_binary(name = "into-md", deps = ["//crates/a"] + all_crate_deps(normal = True), compile_data = ["//web/console:checked_assets"])"#,
        )
        .unwrap();
        for package in ["a", "b"] {
            fs::write(
                root.join(format!("crates/{package}/Cargo.toml")),
                format!("[package]\nname = \"{package}\"\nversion = \"0.0.0\"\n"),
            )
            .unwrap();
        }
        fs::write(
            root.join("crates/a/BUILD.bazel"),
            r#"rust_library(name = "a", deps = ["//crates/b"] + all_crate_deps(normal = True))"#,
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
            r#"rust_library(name = "a", deps = all_crate_deps(normal = True))"#,
        )
        .unwrap();
        validate_bazel_runtime_graph(&root, &mut errors);
        assert!(errors.iter().any(|error| error.contains("runtime closure differs")));
        fs::remove_dir_all(root).unwrap();
    }
}
