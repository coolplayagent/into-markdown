//! npm lock/inventory-backed runtime component authority.

use crate::schema::{Component, IntegrityEvidence, SourceComponent};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema_version: u64,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Package {
    name: String,
    version: String,
    integrity: String,
    license: String,
    source: String,
    scope: String,
    included_in_release: bool,
    license_file: Option<String>,
    license_sha256: Option<String>,
    license_source: Option<String>,
    copyright: Option<String>,
}

pub(crate) fn load(contents: &str, errors: &mut Vec<String>) -> Vec<Component> {
    let inventory = parse(contents, errors);
    inventory
        .packages
        .into_iter()
        .filter(|package| package.included_in_release)
        .map(|package| {
            validate_runtime(&package, errors);
            let package_integrity = integrity(&package);
            Component {
                id: package_id(&package),
                kind: "npm-runtime".to_owned(),
                status: "reviewed".to_owned(),
                included_in_release: true,
                release_eligible: true,
                manual_only: false,
                required_in_core: true,
                version: Some(package.version.clone()),
                source: package.license_source.or(Some(package.source)),
                license: Some(package.license),
                obligations: Some(format!(
                    "Preserve {} and its fixed license text and copyright notice: {}",
                    package.name,
                    package.copyright.clone().unwrap_or_default()
                )),
                integrity: vec![package_integrity],
                authority: "pnpm-lock.yaml + third_party/licenses/npm-inventory.json".to_owned(),
            }
        })
        .collect()
}

pub(crate) fn source_dependencies(
    contents: &str,
    errors: &mut Vec<String>,
) -> Vec<SourceComponent> {
    parse(contents, errors)
        .packages
        .into_iter()
        .filter(|package| !package.included_in_release)
        .map(|package| {
            if !matches!(package.scope.as_str(), "build" | "test")
                || package.version.trim().is_empty()
                || package.source.trim().is_empty()
                || package.license.trim().is_empty()
                || !package.integrity.starts_with("sha512-")
            {
                errors.push(format!(
                    "non-distributed npm dependency {}@{} lacks scope, source, license, or integrity",
                    package.name, package.version
                ));
            }
            let package_integrity = integrity(&package);
            SourceComponent {
                id: package_id(&package),
                kind: format!("npm-{}", package.scope),
                version: package.version.clone(),
                source: package.source.clone(),
                license: package.license.clone(),
                scope: package.scope,
                distributed: false,
                integrity: vec![package_integrity],
                authority: "pnpm-lock.yaml + third_party/licenses/npm-inventory.json".to_owned(),
                files: Vec::new(),
            }
        })
        .collect()
}

fn parse(contents: &str, errors: &mut Vec<String>) -> Inventory {
    let inventory: Inventory = match serde_json::from_str(contents) {
        Ok(inventory) => inventory,
        Err(error) => {
            errors.push(format!("invalid npm release component authority: {error}"));
            return Inventory { schema_version: 0, packages: Vec::new() };
        }
    };
    if inventory.schema_version != 1 {
        errors.push("unsupported npm release component authority schema_version".to_owned());
    }
    inventory
}

fn validate_runtime(package: &Package, errors: &mut Vec<String>) {
    if package.scope != "runtime"
        || !package.integrity.starts_with("sha512-")
        || package.license_file.as_deref().is_none_or(str::is_empty)
        || package.license_sha256.as_deref().is_none_or(str::is_empty)
        || package.license_source.as_deref().is_none_or(str::is_empty)
        || package.copyright.as_deref().is_none_or(str::is_empty)
    {
        errors.push(format!(
            "released npm component {}@{} lacks runtime, integrity, or notice evidence",
            package.name, package.version
        ));
    }
}

fn package_id(package: &Package) -> String {
    format!("npm:{}@{}", package.name, package.version)
}

fn integrity(package: &Package) -> IntegrityEvidence {
    IntegrityEvidence {
        algorithm: "SRI-SHA-512".to_owned(),
        digest: package.integrity.clone(),
        subject: format!("npm tarball {}@{}", package.name, package.version),
        target: None,
    }
}
