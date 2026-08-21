//! Cargo metadata-derived normal dependency authority.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalRuntimeAuthority {
    schema_version: u64,
    root: String,
    cargo_lock_sha256: String,
    workspace_manifest_sha256: BTreeMap<String, String>,
    local_runtime_packages: Vec<String>,
    normal_registry_packages: Vec<String>,
    non_normal_registry_packages: Vec<String>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    resolve: Option<MetadataResolve>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: String,
    license: Option<String>,
    publish: Option<Vec<String>>,
}

pub(crate) struct LocalRuntimePackage {
    pub(crate) license: String,
}

pub(crate) struct RuntimeClosure {
    pub(crate) registry: BTreeSet<(String, String)>,
    pub(crate) local: BTreeMap<(String, String), LocalRuntimePackage>,
}

#[derive(Deserialize)]
struct MetadataResolve {
    nodes: Vec<MetadataNode>,
}

#[derive(Deserialize)]
struct MetadataNode {
    id: String,
    deps: Vec<MetadataDependency>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    pkg: String,
    dep_kinds: Vec<MetadataDependencyKind>,
}

#[derive(Deserialize)]
struct MetadataDependencyKind {
    kind: Option<String>,
}

pub(crate) fn packages(
    repository: &Path,
    lock_text: &str,
    locked: &BTreeSet<(String, String)>,
    authority_text: &str,
    errors: &mut Vec<String>,
) -> RuntimeClosure {
    let authority: NormalRuntimeAuthority = match serde_json::from_str(authority_text) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid Cargo normal-runtime authority: {error}"));
            return RuntimeClosure { registry: BTreeSet::new(), local: BTreeMap::new() };
        }
    };
    if authority.schema_version != 1 || authority.root != "into-markdown-cli" {
        errors.push("Cargo normal-runtime authority has unsupported schema or root".to_owned());
    }
    validate_lock_hash(lock_text.as_bytes(), &authority.cargo_lock_sha256, errors);
    let Some(metadata) = cargo_metadata(repository, errors) else {
        return RuntimeClosure { registry: BTreeSet::new(), local: BTreeMap::new() };
    };
    let expected_manifests = workspace_manifests(repository, &metadata, errors);
    validate_manifest_hashes(
        repository,
        &expected_manifests,
        &authority.workspace_manifest_sha256,
        errors,
    );
    let normal = parse_partition("normal", &authority.normal_registry_packages, errors);
    let non_normal = parse_partition("non-normal", &authority.non_normal_registry_packages, errors);
    if !normal.is_disjoint(&non_normal)
        || normal.union(&non_normal).cloned().collect::<BTreeSet<_>>() != *locked
    {
        errors.push(
            "Cargo normal and non-normal authority must exactly partition registry Cargo.lock"
                .to_owned(),
        );
    }
    let (computed_normal, local) = metadata_normal_packages(repository, &metadata, errors);
    if normal != computed_normal {
        errors.push(
            "Cargo normal package authority differs from cargo metadata normal dependency closure"
                .to_owned(),
        );
    }
    let computed_non_normal = locked.difference(&computed_normal).cloned().collect();
    if non_normal != computed_non_normal {
        errors.push(
            "Cargo non-normal package authority differs from cargo metadata dependency kinds"
                .to_owned(),
        );
    }
    let reviewed_local =
        parse_partition("local runtime", &authority.local_runtime_packages, errors);
    if reviewed_local != local.keys().cloned().collect() {
        errors.push(
            "Cargo local runtime authority differs from cargo metadata normal dependency closure"
                .to_owned(),
        );
    }
    RuntimeClosure { registry: normal, local }
}

fn cargo_metadata(repository: &Path, errors: &mut Vec<String>) -> Option<CargoMetadata> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = match Command::new(cargo)
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .current_dir(repository)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            errors.push(format!("cannot run cargo metadata for release authority: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        errors.push(format!(
            "cargo metadata failed for release authority: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        return None;
    }
    match serde_json::from_slice(&output.stdout) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            errors.push(format!("invalid cargo metadata release authority: {error}"));
            None
        }
    }
}

fn workspace_manifests(
    repository: &Path,
    metadata: &CargoMetadata,
    errors: &mut Vec<String>,
) -> BTreeSet<String> {
    let members: BTreeSet<_> = metadata.workspace_members.iter().collect();
    let mut paths = BTreeSet::from(["Cargo.toml".to_owned()]);
    for package in metadata.packages.iter().filter(|package| members.contains(&package.id)) {
        let manifest = Path::new(&package.manifest_path);
        match manifest.strip_prefix(repository) {
            Ok(relative) => {
                let value = relative.to_string_lossy().replace('\\', "/");
                paths.insert(value);
            }
            Err(_) => errors.push(format!(
                "cargo metadata workspace manifest escapes repository: {}",
                package.manifest_path
            )),
        }
    }
    if paths.len() != metadata.workspace_members.len() + 1 {
        errors
            .push("cargo metadata workspace members do not map one-to-one to manifests".to_owned());
    }
    paths
}

fn metadata_normal_packages(
    repository: &Path,
    metadata: &CargoMetadata,
    errors: &mut Vec<String>,
) -> (BTreeSet<(String, String)>, BTreeMap<(String, String), LocalRuntimePackage>) {
    let Some(resolve) = &metadata.resolve else {
        errors.push("cargo metadata lacks a resolve graph".to_owned());
        return (BTreeSet::new(), BTreeMap::new());
    };
    let packages: BTreeMap<_, _> =
        metadata.packages.iter().map(|package| (package.id.as_str(), package)).collect();
    let nodes: BTreeMap<_, _> = resolve.nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    let roots: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| package.name == "into-markdown-cli" && package.source.is_none())
        .collect();
    let [root] = roots.as_slice() else {
        errors.push("cargo metadata must contain one workspace into-markdown-cli root".to_owned());
        return (BTreeSet::new(), BTreeMap::new());
    };
    let mut visited = BTreeSet::new();
    let mut pending = vec![root.id.as_str()];
    let mut normal = BTreeSet::new();
    let mut local = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(package) = packages.get(id) else {
            errors.push(format!("cargo metadata resolve references unknown package {id}"));
            continue;
        };
        if package.source.as_deref()
            == Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            normal.insert((package.name.clone(), package.version.clone()));
        } else if package.source.is_none() {
            let manifest = Path::new(&package.manifest_path);
            if let Ok(relative) = manifest.strip_prefix(repository) {
                let relative = relative.to_string_lossy().replace('\\', "/");
                if relative.starts_with("third_party/") {
                    let key = (package.name.clone(), package.version.clone());
                    let license = package.license.clone().unwrap_or_default();
                    if license.is_empty() || package.publish.as_deref() != Some(&[]) {
                        errors.push(format!(
                            "local runtime package {}@{} must declare a license and publish=false",
                            key.0, key.1
                        ));
                    }
                    if local.insert(key.clone(), LocalRuntimePackage { license }).is_some() {
                        errors.push(format!("duplicate local runtime package {}@{}", key.0, key.1));
                    }
                }
            }
        }
        let Some(node) = nodes.get(id) else {
            errors.push(format!("cargo metadata lacks resolve node for {id}"));
            continue;
        };
        for dependency in &node.deps {
            if dependency.dep_kinds.iter().any(|kind| kind.kind.is_none()) {
                pending.push(&dependency.pkg);
            }
        }
    }
    (normal, local)
}

fn validate_manifest_hashes(
    repository: &Path,
    expected_paths: &BTreeSet<String>,
    manifests: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    if manifests.keys().cloned().collect::<BTreeSet<_>>() != *expected_paths {
        errors.push(
            "Cargo normal-runtime authority must exactly bind cargo metadata workspace manifests"
                .to_owned(),
        );
    }
    for (path, expected) in manifests {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || relative.file_name().and_then(|name| name.to_str()) != Some("Cargo.toml")
        {
            errors
                .push(format!("Cargo normal-runtime authority has unsafe manifest path {path:?}"));
            continue;
        }
        match std::fs::read(repository.join(relative)) {
            Ok(contents) => match canonical_text_sha256(&contents) {
                Ok(actual) => {
                    if expected.len() != 64
                        || !expected
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                        || actual != *expected
                    {
                        errors.push(format!("Cargo normal-runtime authority is stale for {path}"));
                    }
                }
                Err(error) => errors.push(format!(
                    "Cargo normal-runtime manifest {path} is not canonical UTF-8 text: {error}"
                )),
            },
            Err(error) => {
                errors.push(format!("cannot read Cargo normal-runtime manifest {path}: {error}"));
            }
        }
    }
}

fn canonical_text_sha256(contents: &[u8]) -> Result<String, &'static str> {
    std::str::from_utf8(contents).map_err(|_| "invalid UTF-8")?;
    let mut normalized = Vec::with_capacity(contents.len());
    let mut index = 0;
    while index < contents.len() {
        if contents[index] == b'\r' {
            if contents.get(index + 1) != Some(&b'\n') {
                return Err("isolated carriage return");
            }
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(contents[index]);
            index += 1;
        }
    }
    Ok(format!("{:x}", Sha256::digest(normalized)))
}

fn validate_lock_hash(contents: &[u8], expected: &str, errors: &mut Vec<String>) {
    match canonical_text_sha256(contents) {
        Ok(actual) if actual != expected => {
            errors.push("Cargo normal-runtime authority is stale for Cargo.lock".to_owned());
        }
        Err(error) => errors.push(format!("Cargo.lock is not canonical UTF-8 text: {error}")),
        Ok(_) => {}
    }
}

fn parse_partition(
    kind: &str,
    values: &[String],
    errors: &mut Vec<String>,
) -> BTreeSet<(String, String)> {
    let mut packages = BTreeSet::new();
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|item: &str| item >= value.as_str()) {
            errors.push(format!("Cargo {kind} package authority is duplicate or unsorted"));
        }
        previous = Some(value);
        let Some((name, version)) = value.rsplit_once('@') else {
            errors.push(format!("Cargo {kind} package authority has invalid ID {value:?}"));
            continue;
        };
        if name.is_empty()
            || version.is_empty()
            || !packages.insert((name.to_owned(), version.to_owned()))
        {
            errors.push(format!("Cargo {kind} package authority has invalid ID {value:?}"));
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::{canonical_text_sha256, validate_lock_hash, validate_manifest_hashes};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn manifest_hash_accepts_lf_and_crlf_but_rejects_other_drift() {
        let nonce =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir()
            .join(format!("cargo-manifest-hash-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Cargo.toml");
        let lf = b"[package]\nname = \"fixture\"\n";
        let expected = canonical_text_sha256(lf).unwrap();
        let paths = BTreeSet::from(["Cargo.toml".to_owned()]);
        let manifests = BTreeMap::from([("Cargo.toml".to_owned(), expected)]);

        for contents in [lf.as_slice(), b"[package]\r\nname = \"fixture\"\r\n".as_slice()] {
            std::fs::write(&path, contents).unwrap();
            let mut errors = Vec::new();
            validate_manifest_hashes(&root, &paths, &manifests, &mut errors);
            assert!(errors.is_empty(), "{errors:?}");
        }

        for contents in [
            b"[package]\rname = \"fixture\"\n".as_slice(),
            b"[package]\nname = \"mutated\"\n".as_slice(),
        ] {
            std::fs::write(&path, contents).unwrap();
            let mut errors = Vec::new();
            validate_manifest_hashes(&root, &paths, &manifests, &mut errors);
            assert!(!errors.is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_hash_accepts_lf_and_crlf_but_rejects_invalid_text_and_content() {
        let lf = b"version = 4\n[[package]]\nname = \"fixture\"\n";
        let crlf = b"version = 4\r\n[[package]]\r\nname = \"fixture\"\r\n";
        let expected = canonical_text_sha256(lf).unwrap();

        for contents in [lf.as_slice(), crlf.as_slice()] {
            let mut errors = Vec::new();
            validate_lock_hash(contents, &expected, &mut errors);
            assert!(errors.is_empty(), "{errors:?}");
        }
        for contents in [
            b"version = 4\n[[package]]\nname = \"mutated\"\n".as_slice(),
            b"version = 4\r[[package]]\n".as_slice(),
            b"version = 4\n\xff".as_slice(),
        ] {
            let mut errors = Vec::new();
            validate_lock_hash(contents, &expected, &mut errors);
            assert!(!errors.is_empty());
        }
    }
}
