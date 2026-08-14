//! Cargo metadata-derived normal dependency authority.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalRuntimeAuthority {
    schema_version: u64,
    root: String,
    cargo_lock_sha256: String,
    workspace_manifest_sha256: BTreeMap<String, String>,
    normal_registry_packages: Vec<String>,
    non_normal_registry_packages: Vec<String>,
}

pub(crate) fn packages(
    repository: &Path,
    lock_text: &str,
    locked: &BTreeSet<(String, String)>,
    authority_text: &str,
    errors: &mut Vec<String>,
) -> BTreeSet<(String, String)> {
    let authority: NormalRuntimeAuthority = match serde_json::from_str(authority_text) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid Cargo normal-runtime authority: {error}"));
            return BTreeSet::new();
        }
    };
    if authority.schema_version != 1 || authority.root != "into-markdown-cli" {
        errors.push("Cargo normal-runtime authority has unsupported schema or root".to_owned());
    }
    let lock_hash = format!("{:x}", Sha256::digest(lock_text.as_bytes()));
    if authority.cargo_lock_sha256 != lock_hash {
        errors.push("Cargo normal-runtime authority is stale for Cargo.lock".to_owned());
    }
    validate_manifest_hashes(repository, &authority.workspace_manifest_sha256, errors);
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
    normal
}

fn validate_manifest_hashes(
    repository: &Path,
    manifests: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    if manifests.is_empty() || !manifests.contains_key("Cargo.toml") {
        errors.push("Cargo normal-runtime authority must bind workspace manifests".to_owned());
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
            Ok(contents) => {
                let actual = format!("{:x}", Sha256::digest(contents));
                if expected.len() != 64
                    || !expected
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    || actual != *expected
                {
                    errors.push(format!("Cargo normal-runtime authority is stale for {path}"));
                }
            }
            Err(error) => {
                errors.push(format!("cannot read Cargo normal-runtime manifest {path}: {error}"));
            }
        }
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
