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
            _included_in_release: false,
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
    let cli = std::fs::read_to_string(repository.join("apps/cli/BUILD.bazel")).unwrap_or_default();
    if !cli.contains("all_crate_deps(normal = True)")
        || !cli.contains("compile_data = [\"//web/console:checked_assets\"]")
    {
        errors.push(
            "Bazel into-md target is not bound to Cargo runtime deps and console assets".to_owned(),
        );
    }
    let module = std::fs::read_to_string(repository.join("MODULE.bazel")).unwrap_or_default();
    if !module.contains("crate.from_cargo(")
        || !module.contains("cargo_lockfile = \"//:Cargo.lock\"")
        || !module.contains("manifests = [\"//:Cargo.toml\"]")
    {
        errors
            .push("Bazel crate universe is not bound to the workspace Cargo authority".to_owned());
    }
}
