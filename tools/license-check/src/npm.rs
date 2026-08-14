//! npm lock/inventory-backed runtime component authority.

use crate::schema::Component;
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
    let inventory: Inventory = match serde_json::from_str(contents) {
        Ok(inventory) => inventory,
        Err(error) => {
            errors.push(format!("invalid npm release component authority: {error}"));
            return Vec::new();
        }
    };
    if inventory.schema_version != 1 {
        errors.push("unsupported npm release component authority schema_version".to_owned());
    }
    inventory
        .packages
        .into_iter()
        .filter(|package| package.included_in_release)
        .map(|package| {
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
            Component {
                id: format!("npm:{}@{}", package.name, package.version),
                kind: "npm-runtime".to_owned(),
                status: "reviewed".to_owned(),
                _included_in_release: true,
                manual_only: false,
                version: Some(package.version),
                source: package.license_source.or(Some(package.source)),
                license: Some(package.license),
                obligations: Some(format!(
                    "Preserve {} and its fixed license text and copyright notice.",
                    package.name
                )),
            }
        })
        .collect()
}
